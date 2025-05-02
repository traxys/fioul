use std::{
    cmp::Ordering,
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Read,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use chrono::Utc;
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
use geo::{Distance, Geodesic, Point};
use itertools::Itertools;
use lru::LruCache;
use serde::{Deserialize, Serialize};

const DEFAULT_DISTANCE: f64 = 5.0;
// Default cached duration is one month
const DEFAULT_CACHE_DURATION: Duration = Duration::from_secs(60 * 60 * 24 * 30);

const CACHE_VERSION: u64 = 2;
const LRU_CAPACITY: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

#[derive(Serialize, Deserialize)]
struct GeoInfo {
    display_name: String,
    lat: f64,
    lon: f64,

    expires: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash)]
enum CacheKey {
    Station(u64),
    GeoLocation(String),
}

struct GeoCache {
    path: Option<PathBuf>,
    nominatim: Option<String>,
    cache: LruCache<CacheKey, GeoInfo>,
    duration: Duration,
}

#[derive(Serialize, Deserialize)]
struct OnDiskCache {
    version: u64,
    info: Vec<(CacheKey, GeoInfo)>,
}

#[derive(Deserialize)]
struct OsmInfo {
    display_name: String,
    lat: String,
    lon: String,
}

impl GeoInfo {
    fn from_osm(osm: OsmInfo, duration: Duration) -> color_eyre::Result<Self> {
        Ok(Self {
            display_name: osm.display_name,
            lat: osm.lat.parse()?,
            lon: osm.lon.parse()?,

            expires: chrono::Utc::now() + duration,
        })
    }
}

fn opt_tr<T, U>(a: Option<T>, b: Option<U>) -> Option<(T, U)> {
    Some((a?, b?))
}

impl GeoCache {
    fn get_station(&mut self, station: &Station) -> color_eyre::Result<Option<&GeoInfo>> {
        opt_tr(self.nominatim.as_ref(), station.location.coordinates)
            .map(|(url, coords)| {
                self.cache
                    .try_get_or_insert(CacheKey::Station(station.id), || {
                        let response = ureq::get(&format!("{url}/reverse"))
                            .query("format", "json")
                            .query("lat", (coords.latitude / 100000.).to_string())
                            .query("lon", (coords.longitude / 100000.).to_string())
                            .call()?
                            .body_mut()
                            .read_to_string()?;

                        let response: OsmInfo = match serde_json::from_str(&response) {
                            Ok(v) => v,
                            Err(e) => {
                                eprintln!("Response was:\n{response}");
                                return Err(e).wrap_err("could not get osm information");
                            }
                        };

                        GeoInfo::from_osm(response, self.duration)
                    })
            })
            .transpose()
    }

    fn get_query(&mut self, query: &str) -> color_eyre::Result<&GeoInfo> {
        match &self.nominatim {
            Some(url) => self
                .cache
                .try_get_or_insert(CacheKey::GeoLocation(query.into()), || {
                    let response = ureq::get(&format!("{url}/search"))
                        .query("q", query)
                        .query("limit", "1")
                        .query("countrycodes", "fr")
                        .call()?
                        .body_mut()
                        .read_to_string()?;

                    let response: Vec<OsmInfo> = match serde_json::from_str(&response) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("Response was:\n{response}");
                            return Err(e).wrap_err("could not get osm information");
                        }
                    };

                    match response.into_iter().next() {
                        None => color_eyre::eyre::bail!("Query '{query}' returned no results"),
                        Some(v) => GeoInfo::from_osm(v, self.duration),
                    }
                }),
            None => color_eyre::eyre::bail!(
                "Nominatim URL must be provided when using queried locations"
            ),
        }
    }

    fn non_backed(nominatim: Option<String>, duration: Duration) -> Self {
        Self {
            nominatim,
            path: None,
            cache: LruCache::new(LRU_CAPACITY),
            duration,
        }
    }

    fn empty(from: &Path, nominatim: Option<String>, duration: Duration) -> Self {
        Self {
            nominatim,
            path: Some(from.into()),
            cache: LruCache::new(LRU_CAPACITY),
            duration,
        }
    }

    fn load(
        from: &Path,
        nominatim: Option<String>,
        duration: Duration,
    ) -> color_eyre::Result<Self> {
        let data: ciborium::Value = ciborium::from_reader(File::open(from)?)?;
        let map = data
            .as_map()
            .ok_or(color_eyre::eyre::eyre!("Data is not a map"))?;
        let (_, version) = map
            .iter()
            .find(|(k, _)| k == &ciborium::Value::Text("version".into()))
            .ok_or(color_eyre::eyre::eyre!("Version is not present"))?;
        let version: u64 = version
            .clone()
            .into_integer()
            .map_err(|_| color_eyre::eyre::eyre!("version is not an integer"))?
            .try_into()?;

        color_eyre::eyre::ensure!(
            version == CACHE_VERSION,
            "Cache at {from:?} is using version {version}, while version {CACHE_VERSION} was expected."
        );

        let data: OnDiskCache = data.deserialized()?;

        let mut cache = LruCache::new(LRU_CAPACITY);

        for (id, info) in data
            .info
            .into_iter()
            .rev()
            .filter(|(_, i)| i.expires > Utc::now())
        {
            cache.put(id, info);
        }

        Ok(Self {
            nominatim,
            path: Some(from.into()),
            cache,
            duration,
        })
    }

    fn store(self) -> color_eyre::Result<()> {
        if let Some(p) = self.path {
            let on_disk = OnDiskCache {
                version: CACHE_VERSION,
                info: self.cache.into_iter().collect(),
            };

            let file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(p)
                .wrap_err("Could not create cache file")?;

            ciborium::into_writer(&on_disk, file)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SortCriteria {
    Price,
    Distance,
}

#[derive(clap::Parser, Debug)]
struct Args {
    #[arg(short = 'u', long = "url", env = "FL_SERVER_URL", global = true)]
    server_url: Option<String>,
    #[arg(short, long, help = "Criteria to sort with", global = true)]
    sort: Option<SortCriteria>,
    #[arg(
        short = 'N',
        long,
        env = "FL_NOMINATIM_URL",
        help = "URL to a nominatim server",
        global = true
    )]
    nominatim: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Location {
    Coords { latitude: f64, longitude: f64 },
    Name(String),
}

impl FromStr for Location {
    type Err = color_eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(query) = s.strip_prefix("q:") {
            Ok(Self::Name(query.into()))
        } else {
            let Some((latitude, longitude)) = s.split_once(',') else {
                color_eyre::eyre::bail!("expected a location of the form <latitude>,<longitude>")
            };

            Ok(Self::Coords {
                latitude: latitude.parse()?,
                longitude: longitude.parse()?,
            })
        }
    }
}

impl Location {
    fn to_coords(&self, cache: &mut GeoCache) -> color_eyre::Result<(f64, f64)> {
        match self {
            &Location::Coords {
                latitude,
                longitude,
            } => Ok((latitude, longitude)),
            Location::Name(query) => {
                let info = cache.get_query(query)?;

                Ok((info.lat, info.lon))
            }
        }
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    Near(Near),
    Profile(DoProfile),
}

fn mk_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug)]
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

#[derive(Serialize, Deserialize, Default, Debug)]
struct SortConfig {
    fuel: Option<Fuel>,
}

#[derive(Serialize, Deserialize, Debug)]
struct StationInfo {
    name: String,
    id: u64,
}

#[derive(Serialize, Deserialize, Debug)]
struct Profile {
    #[serde(default)]
    fuels: Vec<fioul::Fuel>,
    stations: Vec<StationInfo>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct DefaultValueConfig {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    location: Option<Location>,
    #[serde(default)]
    distance: Option<f64>,
    #[serde(default)]
    nominatim: Option<String>,
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    cache_duration: Option<Duration>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct Config {
    #[serde(default)]
    default: DefaultValueConfig,
    #[serde(default)]
    display: DisplayConfig,
    #[serde(default)]
    sort: SortConfig,
    #[serde(default)]
    profiles: HashMap<String, Profile>,
}

#[derive(clap::Args, Debug)]
struct Near {
    #[arg(help = "Location of the form <latitude>,<longitude> or `q:<nominatim search query>`.")]
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
    let mut response = ureq::get(&format!("{url}/api/stations"))
        .config()
        .http_status_as_error(false)
        .build()
        .query_pairs(queries)
        .call()?;

    if !response.status().is_success() {
        let err_rsp = response
            .body_mut()
            .read_json::<fioul_types::Response<Void>>();

        match err_rsp {
            Ok(v) => match v {
                fioul_types::Response::Error { code: _, message } => {
                    color_eyre::eyre::bail!("Error: {message}")
                }
                _ => unreachable!(),
            },
            Err(_) => {
                color_eyre::eyre::bail!(
                    "Server returned status code {}, and no response",
                    response.status()
                )
            }
        }
    } else {
        let stations: fioul_types::Response<Stations> = response.body_mut().read_json()?;

        match stations {
            fioul_types::Response::Error { .. } => unreachable!(),
            fioul_types::Response::Ok(v) => Ok(v.stations),
        }
    }
}

fn display_fuel(fuel: fioul::Fuel, price: &Price, display_dates: bool) {
    print!("  - {fuel}: {}", price.price);
    if display_dates {
        print!(" (at {})", price.updated_at);
    }
    println!()
}

fn print_stations(
    stations: &[Station],
    config: &Config,
    cache: &mut GeoCache,
) -> color_eyre::Result<()> {
    for station in stations {
        let geo_info = cache.get_station(station)?;

        println!(
            "== {} - {} ({})",
            station.location.city.as_deref().unwrap_or(""),
            station.location.address.as_deref().unwrap_or(""),
            station.location.zip_code,
        );
        if let Some(info) = geo_info {
            if let Some(name) = info.display_name.split(',').next() {
                println!("Name: {name}");
            }
        }
        println!("ID: {}", station.id);
        println!("Prices:");

        if config.display.fuels.is_empty() {
            for (fuel, price) in &station.prices {
                price
                    .iter()
                    .for_each(|p| display_fuel(fuel, p, config.display.dates))
            }
        } else {
            for &fuel in &config.display.fuels {
                station.prices[fuel]
                    .iter()
                    .for_each(|p| display_fuel(fuel, p, config.display.dates))
            }
        }
        println!()
    }

    Ok(())
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
            &Sort::Distance(from) => stations.sort_unstable_by(|a, b| {
                match (a.location.coordinates, b.location.coordinates) {
                    (None, None) => Ordering::Equal,
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(a), Some(b)) => {
                        let a = Geodesic.distance(
                            from,
                            Point::new(a.latitude / 100000., a.longitude / 100000.),
                        );
                        let b = Geodesic.distance(
                            from,
                            Point::new(b.latitude / 100000., b.longitude / 100000.),
                        );

                        a.partial_cmp(&b).expect("distances should be numbers")
                    }
                }
            }),
        }
    }
}

impl Near {
    fn main(
        self,
        url: &str,
        sort: &Sort,
        config: &Config,
        cache: &mut GeoCache,
    ) -> color_eyre::Result<()> {
        let Some((lat, lon)) = self
            .location
            .as_ref()
            .or(config.default.location.as_ref())
            .map(|l| l.to_coords(cache))
            .transpose()?
        else {
            color_eyre::eyre::bail!("No location provided")
        };

        let distance = self
            .distance
            .or(config.default.distance)
            .unwrap_or(DEFAULT_DISTANCE)
            * 1000.0;

        let mut stations = get_stations(
            url,
            [("location", format!("{},{},{}", lat, lon, distance).as_str())],
        )?;

        sort.sort(&mut stations);

        print_stations(&stations, config, cache)
    }
}

#[derive(clap::Args, Debug)]
struct DoProfile {
    #[arg(help = "profile to load")]
    profile: String,
}

impl DoProfile {
    fn main(&self, url: &str, sort: &Sort, config: &Config) -> color_eyre::Result<()> {
        let profile = config
            .profiles
            .get(&self.profile)
            .ok_or(color_eyre::eyre::eyre!(
                "No such profile in the configuration"
            ))?;

        let mut stations = get_stations(
            url,
            [(
                "ids",
                profile
                    .stations
                    .iter()
                    .map(|s| s.id.to_string())
                    .join(",")
                    .as_str(),
            )],
        )?;

        sort.sort(&mut stations);

        for station in stations {
            let name = &profile
                .stations
                .iter()
                .find(|s| s.id == station.id)
                .expect("returned station was not asked")
                .name;

            println!("== {name}");
            for &fuel in &profile.fuels {
                station.prices[fuel]
                    .iter()
                    .for_each(|p| display_fuel(fuel, p, config.display.dates))
            }
            println!()
        }

        Ok(())
    }
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();

    let project_dir = ProjectDirs::from("net", "traxys", "fioul");

    let (config, mut cache) = match &project_dir {
        None => (
            Config::default(),
            GeoCache::non_backed(args.nominatim, DEFAULT_CACHE_DURATION),
        ),
        Some(p) => {
            let config_path = p.config_dir();
            std::fs::create_dir_all(config_path).wrap_err("could not create config directory")?;

            let config_path = config_path.join("config.toml");
            let config: Config = if config_path.exists() {
                let mut config_file = File::open(config_path)?;
                let mut config = String::new();
                config_file.read_to_string(&mut config)?;

                toml::from_str(&config)?
            } else {
                Config::default()
            };

            let nominatim = config.default.nominatim.clone().or(args.nominatim);
            let duration = config
                .default
                .cache_duration
                .unwrap_or(DEFAULT_CACHE_DURATION);

            std::fs::create_dir_all(p.cache_dir()).wrap_err("could not create cache directory")?;

            let cache_path = p.cache_dir().join("geo.cbor");
            let cache = match cache_path.exists() {
                true => GeoCache::load(&cache_path, nominatim, duration)?,
                false => GeoCache::empty(&cache_path, nominatim, duration),
            };

            (config, cache)
        }
    };

    let url = match args.server_url.or(config.default.server.clone()) {
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
            let location = config.default.location.clone().ok_or(
                color_eyre::eyre::eyre!(
                    "Can only sort with distance when 'default.location' is filled in the configuration'"
                ))?;
            let (lat, lon) = location.to_coords(&mut cache)?;
            Sort::Distance(Point::new(lat, lon))
        }
        None => Sort::None,
    };

    let res = match args.command {
        Command::Near(n) => n.main(&url, &sort, &config, &mut cache),
        Command::Profile(p) => p.main(&url, &sort, &config),
    };

    cache.store()?;

    res
}
