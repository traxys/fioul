use std::{
    cmp::Ordering,
    fs::{File, OpenOptions},
    io::Read,
    str::FromStr,
};

use clap::{
    builder::{PossibleValuesParser, TypedValueParser},
    Parser, Subcommand, ValueEnum,
};
use color_eyre::eyre::Context;
use directories::ProjectDirs;
use fioul_types::{
    fioul::{self, Fuel, Price, Station},
    Stations,
};
use geo::{GeodesicDistance, Point};
use serde::{Deserialize, Serialize};

const DEFAULT_DISTANCE: f64 = 5.0;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SortCriteria {
    Price,
    Distance,
}

#[derive(clap::Parser, Debug)]
struct Args {
    #[arg(short = 'u', long = "url", env = "FL_SERVER_URL")]
    server_url: Option<String>,
    #[arg(short, long, help = "Criteria to sort with", global = true)]
    sort: Option<SortCriteria>,
    #[arg(
        short,
        long,
        help = "Fuel to sort with if --sort=price", 
        global = true,
        value_parser = PossibleValuesParser::new([
            "diesel",
            "95",
            "98",
            "E10",
            "E85",
            "LPG",
        ]).map(|v| {
            match v.as_str() {
                "diesel" => Fuel::Diesel,
                "95" => Fuel::Gasoline95Octane,
                "98" => Fuel::Gasoline98Octane,
                "E10" => Fuel::GasolineE10,
                "LPG" => Fuel::LPG,
                "E85" => Fuel::GasolineE85,
                _ => unreachable!(),
            }
        })
    )]
    fuel: Option<Fuel>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Location {
    pub latitude: f64,
    pub longitude: f64,
}

impl FromStr for Location {
    type Err = color_eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((latitude, longitude)) = s.split_once(',') else {
            color_eyre::eyre::bail!("expected a location of the form <latitude>,<longitude>")
        };

        Ok(Self {
            latitude: latitude.parse()?,
            longitude: longitude.parse()?,
        })
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    Near(Near),
}

fn mk_true() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
struct DisplayConfig {
    #[serde(default = "mk_true")]
    dates: bool,
    #[serde(default)]
    fuels: Vec<fioul::Fuel>,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            dates: true,
            fuels: Default::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct SortConfig {
    fuel: Option<Fuel>,
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    default_server: Option<String>,
    default_location: Option<Location>,
    default_distance: Option<f64>,
    #[serde(default)]
    display: DisplayConfig,
    #[serde(default)]
    sort: SortConfig,
}

#[derive(clap::Args, Debug)]
struct Near {
    #[arg(help = "Location of the form <latitude>,<longitude>.")]
    location: Option<Location>,
    #[arg(
        short,
        long,
        help = "Maximum distance of a station from the location (in km). Defaults to 5"
    )]
    distance: Option<f64>,
}

#[derive(Deserialize)]
enum Void {}

fn get_stations<'a, P>(url: &str, queries: P) -> color_eyre::Result<Vec<Station>>
where
    P: IntoIterator<Item = (&'a str, &'a str)>,
{
    let response = ureq::get(&format!("{url}/api/stations"))
        .query_pairs(queries)
        .call();

    let stations: fioul_types::Response<Stations> = match response {
        Ok(v) => v.into_json()?,
        Err(e) => match e {
            t @ ureq::Error::Transport { .. } => return Err(t.into()),
            ureq::Error::Status(code, rsp) => {
                let err_rsp = rsp.into_json::<fioul_types::Response<Void>>();

                match err_rsp {
                    Ok(v) => match v {
                        fioul_types::Response::Error { code: _, message } => {
                            color_eyre::eyre::bail!("Error: {message}")
                        }
                        _ => unreachable!(),
                    },
                    Err(_) => color_eyre::eyre::bail!(
                        "Server returned status code {code}, and no response"
                    ),
                }
            }
        },
    };

    match stations {
        fioul_types::Response::Error { .. } => unreachable!(),
        fioul_types::Response::Ok(v) => Ok(v.stations),
    }
}

fn print_stations(stations: &[Station], config: &Config) {
    for station in stations {
        println!(
            "== {} - {} ({})",
            station.location.city.as_deref().unwrap_or(""),
            station.location.address,
            station.location.zip_code,
        );
        println!("Prices:");

        let display_fuel = |fuel, price: &Price| {
            print!("  - {fuel}: {}", price.price);
            if config.display.dates {
                print!(" (at {})", price.updated_at);
            }
            println!()
        };

        if config.display.fuels.is_empty() {
            for (fuel, price) in &station.prices {
                price.iter().for_each(|p| display_fuel(fuel, p))
            }
        } else {
            for &fuel in &config.display.fuels {
                station.prices[fuel]
                    .iter()
                    .for_each(|p| display_fuel(fuel, p))
            }
        }
        println!()
    }
}

enum Sort {
    None,
    Distance(Point),
    Price(Fuel),
}

impl Sort {
    fn sort(&self, stations: &mut [Station]) {
        match self {
            Sort::None => (),
            &Sort::Price(f) => {
                let later_date = |date: fioul::DateTime| match date {
                    fioul::DateTime::Single(a) => a,
                    fioul::DateTime::Ambiguous(a, b) => std::cmp::max(a, b),
                };

                let latest_price = |p: &[Price]| {
                    p.iter()
                        .max_by_key(|p| later_date(p.updated_at))
                        .expect("slice must not be empty")
                        .price
                };

                stations.sort_unstable_by(|a, b| {
                    match (a.prices[f].as_slice(), b.prices[f].as_slice()) {
                        ([], []) => Ordering::Equal,
                        ([_, ..], []) => Ordering::Less,
                        ([], [_, ..]) => Ordering::Greater,
                        (a, b) => latest_price(a)
                            .partial_cmp(&latest_price(b))
                            .expect("prices may not be NaN/Inf"),
                    }
                });
            }
            Sort::Distance(from) => stations.sort_unstable_by(|a, b| {
                match (a.location.coordinates, b.location.coordinates) {
                    (None, None) => Ordering::Equal,
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(a), Some(b)) => {
                        let a = Point::new(a.latitude / 100000., a.longitude / 100000.)
                            .geodesic_distance(from);
                        let b = Point::new(b.latitude / 100000., b.longitude / 100000.)
                            .geodesic_distance(from);

                        a.partial_cmp(&b).expect("distances should be numbers")
                    }
                }
            }),
        }
    }
}

impl Near {
    fn main(self, url: &str, sort: &Sort, config: &Config) -> color_eyre::Result<()> {
        let Some(location) = self.location.or(config.default_location) else {
            color_eyre::eyre::bail!("No location provided")
        };

        let distance = self
            .distance
            .or(config.default_distance)
            .unwrap_or(DEFAULT_DISTANCE)
            * 1000.0;

        let mut stations = get_stations(
            url,
            [(
                "location",
                format!("{},{},{}", location.latitude, location.longitude, distance).as_str(),
            )],
        )?;

        sort.sort(&mut stations);

        print_stations(&stations, config);
        Ok(())
    }
}

fn config_file(project: &ProjectDirs) -> color_eyre::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(project.config_dir().join("config.toml"))
        .wrap_err("could not open config file")
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();

    let project_dir = ProjectDirs::from("net", "traxys", "fioul");

    let config = match &project_dir {
        None => Config::default(),
        Some(p) => {
            let config_path = p.config_dir();
            std::fs::create_dir_all(config_path).wrap_err("could not create config directory")?;

            let mut config_file = config_file(p)?;

            let mut config = String::new();
            config_file.read_to_string(&mut config)?;

            toml::from_str(&config)?
        }
    };

    let url = match args.server_url.or(config.default_server.clone()) {
        None => {
            color_eyre::eyre::bail!("No server was provided. Server can be provided either through the command line arguments or the config file.")
        }
        Some(e) => e,
    };

    let sort = match args.sort {
        Some(SortCriteria::Price) => {
            let fuel_sort = args
                .fuel
                .or(config.sort.fuel)
                .ok_or(color_eyre::eyre::eyre!(
                    "A fuel type must be specified to be able to sort by price"
                ))?;
            Sort::Price(fuel_sort)
        }
        Some(SortCriteria::Distance) => {
            let location = config.default_location.ok_or(
                color_eyre::eyre::eyre!(
                    "Can only sort with distance when 'default_location' is filled in the configuration'"
                ))?;
            Sort::Distance(Point::new(location.latitude, location.longitude))
        }
        None => Sort::None,
    };

    match args.command {
        Command::Near(n) => n.main(&url, &sort, &config),
    }
}
