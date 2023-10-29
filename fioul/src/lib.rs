//!
//! This crate fetches and parses the data for french gas stations. This data is provided on
//! [by the government](https://www.prix-carburants.gouv.fr/rubrique/opendata/), and is
//! distributed as an ZIP archive containing an XML file.
//!
//! The entry point of the crate is [Parser::new] that allows to fetch the different available
//! informations.
//!
//! Due to the ambiguity and lack of consistency of the data it is possible to tune some options
//! when viewing different kind of sources through [Parser::with_options].
//!
//! Note that data is provided since 2007, but the schema has changed since and this crate
//! is not (yet?) able to parse all historical data.
//!

use std::{
    fs::File,
    io::{Cursor, Read, Seek},
    num::{ParseFloatError, ParseIntError},
    path::Path,
};

use aho_corasick::AhoCorasick;
use chrono::{LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Europe::Paris;
use encoding_rs_io::DecodeReaderBytesBuilder;
use enum_map::{Enum, EnumMap};
use roxmltree::Node;
use zip::{result::ZipError, ZipArchive};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("error while fetching data")]
    Network(#[from] reqwest::Error),
    #[error("error while reading data")]
    IO(#[from] std::io::Error),
    #[error("error while reading data from archive")]
    Archive(#[from] ZipError),
    #[error("fetched archive is empty")]
    NoData,
    #[error("could not parse XML document")]
    Parsing(#[from] roxmltree::Error),
    #[error("document is missing the field '{node}' at path {path}")]
    MissingNode { path: String, node: String },
    #[error("unknown node was encountered '{node}' at path {path}")]
    UnhandledNode { path: String, node: String },
    #[error("node at '{0}' is missing a text value")]
    MissingValue(String),
    #[error("missing attribute '{attribute}' at {path}")]
    MissingAttribute {
        path: String,
        attribute: &'static str,
    },
    #[error("did not expect node '{got}' at {path}, expected {expected}")]
    UnexpectedNode {
        path: String,
        got: String,
        expected: &'static str,
    },
    #[error("booleans are expected to be either '1' or ''")]
    MalformedBool(String),
    #[error("could not parse int at {path}")]
    InvalidInt {
        path: String,
        #[source]
        err: ParseIntError,
    },
    #[error("could not parse float at {path}")]
    InvalidFloat {
        path: String,
        #[source]
        err: ParseFloatError,
    },
    #[error("fuel type at {path} is unknown: {fuel}")]
    UnknownFuelType { path: String, fuel: String },
    #[error("could not parse date at {path}")]
    InvalidDate {
        path: String,
        #[source]
        err: chrono::ParseError,
    },
    #[error("time of day is not of the form HH.MM at {0}")]
    InvalidTimeOfDay(String),
    #[error("datetime does not exist at {0}")]
    InexistentDateTime(String),
    #[error("closing type at {0} is invalid")]
    InvalidClosingType(String),
    #[error("invalid road kind at {0}")]
    InvalidRoadKind(String),
    #[error("file was not found")]
    NotFound,
}

type Result<T, E = Error> = std::result::Result<T, E>;

fn value(node: Node, path: impl Fn() -> String) -> Result<String> {
    node.text()
        .ok_or_else(|| Error::MissingValue(path()))
        .map(String::from)
}

fn attribute(node: Node, attr: &'static str, path: impl Fn() -> String) -> Result<String> {
    attribute_mapped(node, attr, path, |v, _| String::from(v))
}

fn attribute_mapped<'a, 'input, F, T, P>(
    node: Node<'a, 'input>,
    attr: &'static str,
    path: P,
    f: F,
) -> Result<T>
where
    P: Fn() -> String,
    F: Fn(&'a str, P) -> T,
{
    node.attribute(attr)
        .ok_or_else(|| Error::MissingAttribute {
            path: path(),
            attribute: attr,
        })
        .map(|v| f(v, path))
}

fn empty_bool(s: &str, path: impl Fn() -> String) -> Result<bool> {
    match s {
        "1" => Ok(true),
        "" => Ok(false),
        _ => Err(Error::MalformedBool(path())),
    }
}

fn int_attr(node: Node, attr: &'static str, path: impl Fn() -> String) -> Result<i64> {
    attribute_mapped(node, attr, path, |v, path| {
        v.parse().map_err(|err| {
            log::warn!("Invalid int for {} at {}: {}", attr, path(), v);
            Error::InvalidInt { path: path(), err }
        })
    })?
}

fn float_attr(node: Node, attr: &'static str, path: impl Fn() -> String) -> Result<f64> {
    attribute_mapped(node, attr, path, |v, path| {
        v.parse().map_err(|err| {
            log::warn!("Invalid float for {} at {}: {}", attr, path(), v);
            Error::InvalidFloat { path: path(), err }
        })
    })?
}

fn bool_attr(node: Node, attr: &'static str, path: impl Fn() -> String) -> Result<bool> {
    attribute_mapped(node, attr, path, empty_bool)?
}

fn fuel_attr(node: Node, attr: &'static str, path: impl Fn() -> String) -> Result<Fuel> {
    attribute_mapped(node, attr, path, Fuel::parse)?
}

fn time_of_day_attr(
    node: Node,
    attr: &'static str,
    path: impl Fn() -> String,
) -> Result<TimeOfDay> {
    attribute_mapped(node, attr, path, |v, path| {
        let Some((hour, minute)) = v.split_once('.') else {
            return Err(Error::InvalidTimeOfDay(path()));
        };

        let parse = |v: &str| {
            v.parse()
                .map_err(|err| Error::InvalidInt { path: path(), err })
        };

        Ok(TimeOfDay {
            hour: parse(hour)?,
            minute: parse(minute)?,
        })
    })?
}

/// Tries to represent a single date, but as France uses daylight saving time
/// dates between 2 and 3 in the morning on the day the clock moves back are ambiguous.
/// This type contains either the single non-ambiguous date, or both possible dates.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DateTime {
    Single(chrono::DateTime<Utc>),
    Ambiguous(chrono::DateTime<Utc>, chrono::DateTime<Utc>),
}

fn parse_date(v: &str, format: DateFormat, path: impl Fn() -> String) -> Result<DateTime> {
    let fmt_str = format!(
        "%F{}%T",
        match format {
            DateFormat::Spaced => " ",
            DateFormat::TSeparated => "T",
        }
    );
    match Paris.from_local_datetime(
        &NaiveDateTime::parse_from_str(v, &fmt_str)
            .map_err(|err| Error::InvalidDate { path: path(), err })?,
    ) {
        LocalResult::None => Err(Error::InexistentDateTime(path())),
        LocalResult::Single(d) => Ok(DateTime::Single(d.with_timezone(&Utc))),
        LocalResult::Ambiguous(a, b) => Ok(DateTime::Ambiguous(
            a.with_timezone(&Utc),
            b.with_timezone(&Utc),
        )),
    }
}

fn date_attr(
    node: Node,
    attr: &'static str,
    path: impl Fn() -> String,
    format: DateFormat,
) -> Result<DateTime> {
    attribute_mapped(node, attr, path, |v, path| parse_date(v, format, path))?
}

/// Main entry point of the crate, storing the parsing options.
pub struct Parser {
    strict: bool,
    re_encode: bool,
    date_format: Option<DateFormat>,
}

/// This struct allows to customize the parsing options, with [Parser::with_options].
pub struct ParserBuilder {
    strict: bool,
    re_encode: bool,
    date_format: Option<DateFormat>,
}

/// Represents a time in the day, like 13:30. Works in 24 hours.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
}

/// Represents an interval in the day, during which a [Station] is open.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpenedHours {
    pub opening: TimeOfDay,
    pub closing: TimeOfDay,
}

/// Represents the status of the [Station] on this particular day.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Hours {
    Closed,
    Open(Vec<OpenedHours>),
}

/// Represents the opening times of the [Station] during the week.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Schedule {
    /// Does the station have an automated booth for 24/24 service
    pub automatic: bool,
    /// The manned booth openings for the week (starting on Monday)
    pub days: [Hours; 7],
}

/// Kinds of fuels available in the [Station]
#[derive(Enum, Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Fuel {
    Diesel,
    Gasoline95Octane,
    Gasoline98Octane,
    GasolineE10,
    LPG,
    GasolineE85,
}

impl Fuel {
    fn parse(v: &str, path: impl Fn() -> String) -> Result<Self> {
        match v {
            "Gazole" => Ok(Fuel::Diesel),
            "SP95" => Ok(Fuel::Gasoline95Octane),
            "SP98" => Ok(Fuel::Gasoline98Octane),
            "GPLc" => Ok(Fuel::LPG),
            "E10" => Ok(Fuel::GasolineE10),
            "E85" => Ok(Fuel::GasolineE85),
            _ => Err(Error::UnknownFuelType {
                path: path(),
                fuel: v.into(),
            }),
        }
    }
}

/// Kind of format the date is stored in the XML. This depends on the source of the data,
/// instantaneous & archived feeds don't use the same dates. The
/// [Parser::fetch_daily]/[Parser::fetch_instant]/... functions will correctly set the date. When
/// reading directly from downloaded files this should be correctly set with [Parser::with_options]
#[derive(Clone, Copy, Debug)]
pub enum DateFormat {
    /// This format seems to be only used for the instantaneous feed
    Spaced,
    /// This format is seemingly used for all other feeds, and is the default when not specified.
    TSeparated,
}

/// Represent the price of a [Fuel] at a particular point in time
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Price {
    pub updated_at: DateTime,
    pub price: f64,
}

/// Represent a shortage of a [Fuel] at a particular point in time. Can be ongoing.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Shortage {
    pub start: DateTime,
    pub end: Option<DateTime>,
}

/// Represent a interval in time in which a [Station] was closed. Can be ongoing.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Closing {
    pub temporary: bool,
    pub start: DateTime,
    pub end: Option<DateTime>,
}

/// Represents physical coordinates, in `PTV_GEODECIMAL`. To obtain `WGS84` coordinates they must
/// be divided by 100000.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Coordinates {
    pub latitude: f64,
    pub longitude: f64,
}

/// Kind of road a [Station] is attached to
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RoadKind {
    Highway,
    Regular,
    None,
}

/// Physical location of a [Station]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Location {
    pub road: RoadKind,
    pub zip_code: String,
    pub address: String,
    pub city: Option<String>,
    pub coordinates: Option<Coordinates>,
}

/// Represents all the information on a single gas station
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Station {
    pub id: String,
    pub location: Location,
    pub prices: EnumMap<Fuel, Vec<Price>>,
    pub shortages: EnumMap<Fuel, Vec<Shortage>>,
    pub schedule: Option<Schedule>,
    pub services: Vec<String>,
    pub closings: Vec<Closing>,
}

type Parsed = Result<Vec<Station>>;

impl Parser {
    /// Create a [ParserBuilder] to be able to customize the options. See it's documentation for
    /// more information on options.
    pub fn with_options() -> ParserBuilder {
        ParserBuilder {
            strict: true,
            re_encode: true,
            date_format: None,
        }
    }

    /// Create a [Parser] with default options
    pub fn new() -> Self {
        Self {
            strict: true,
            re_encode: true,
            date_format: None,
        }
    }

    fn parse_opening<P>(&self, node: Node, path: P) -> Result<OpenedHours>
    where
        P: Fn() -> String + Copy,
    {
        let opening = time_of_day_attr(node, "ouverture", path)?;
        let closing = time_of_day_attr(node, "fermeture", path)?;

        Ok(OpenedHours { opening, closing })
    }

    fn parse_schedule<P>(&self, node: Node, path: P) -> Result<Schedule>
    where
        P: Fn() -> String + Copy,
    {
        let mut days: [Option<_>; 7] = Default::default();

        let automatic = bool_attr(node, "automate-24-24", path)?;

        for (i, day) in node.children().filter(|n| n.is_element()).enumerate() {
            if !day.has_tag_name("jour") {
                return Err(Error::UnexpectedNode {
                    path: path(),
                    got: day.tag_name().name().into(),
                    expected: "jour",
                });
            }

            let day_path = || path() + &format!(".jour[{i}]");

            let id = int_attr(day, "id", day_path)?;
            let closed = bool_attr(day, "ferme", day_path)?;

            let day = match closed {
                true => Hours::Closed,
                false => {
                    let mut openings = Vec::new();

                    for (i, opening) in day.children().filter(|n| n.is_element()).enumerate() {
                        if !opening.has_tag_name("horaire") {
                            return Err(Error::UnexpectedNode {
                                path: day_path(),
                                got: opening.tag_name().name().into(),
                                expected: "horaire",
                            });
                        }

                        openings.push(
                            self.parse_opening(opening, || day_path() + &format!(".horaire[{i}]"))?,
                        );
                    }

                    Hours::Open(openings)
                }
            };

            days[id as usize - 1] = Some(day);
        }

        Ok(Schedule {
            automatic,
            days: days
                .into_iter()
                .enumerate()
                .map(|(i, day)| {
                    day.ok_or_else(|| Error::MissingNode {
                        path: path(),
                        node: format!("jour[{i}]"),
                    })
                })
                .collect::<Result<Vec<_>>>()?
                .try_into()
                .unwrap(),
        })
    }

    fn parse_price<P>(&self, node: Node, path: P, date_format: DateFormat) -> Result<(Fuel, Price)>
    where
        P: Fn() -> String + Copy,
    {
        let fuel = fuel_attr(node, "nom", path)?;
        let updated_at = date_attr(node, "maj", path, date_format)?;
        let price = float_attr(node, "valeur", path)?;

        Ok((fuel, Price { updated_at, price }))
    }

    fn parse_shortage<P>(
        &self,
        node: Node,
        path: P,
        date_format: DateFormat,
    ) -> Result<(Fuel, Shortage)>
    where
        P: Fn() -> String + Copy,
    {
        let fuel = fuel_attr(node, "nom", path)?;
        let start = date_attr(node, "debut", path, date_format)?;
        let end = attribute_mapped(node, "fin", path, |v, path| match v {
            "" => Ok(None),
            v => parse_date(v, date_format, path).map(Some),
        })??;

        Ok((fuel, Shortage { start, end }))
    }

    fn parse_closing<P>(&self, node: Node, path: P, date_format: DateFormat) -> Result<Closing>
    where
        P: Fn() -> String + Copy,
    {
        let temporary = attribute_mapped(node, "type", path, |v, path| match v {
            "T" => Ok(true),
            "D" => Ok(false),
            _ => Err(Error::InvalidClosingType(path())),
        })??;
        let start = date_attr(node, "debut", path, date_format)?;
        let end = attribute_mapped(node, "fin", path, |v, path| match v {
            "" => Ok(None),
            v => parse_date(v, date_format, path).map(Some),
        })??;

        Ok(Closing {
            temporary,
            start,
            end,
        })
    }

    fn parse_services(&self, node: Node, path: impl Fn() -> String) -> Result<Vec<String>> {
        let mut services = Vec::new();

        for (i, service) in node.children().filter(|n| n.is_element()).enumerate() {
            if !service.has_tag_name("service") {
                return Err(Error::UnexpectedNode {
                    path: path(),
                    got: service.tag_name().name().into(),
                    expected: "service",
                });
            }

            services.push(value(service, || path() + &format!(".service[{i}]"))?)
        }

        Ok(services)
    }

    fn parse_location<P>(
        &self,
        node: Node,
        path: P,
        city: Option<String>,
        address: Option<String>,
    ) -> Result<Location>
    where
        P: Fn() -> String + Copy,
    {
        let unwrap = |opt: Option<_>, child: &str| {
            opt.ok_or_else(|| Error::MissingNode {
                path: path(),
                node: child.into(),
            })
        };

        let opt_f64 = |attr| {
            attribute_mapped(node, attr, path, |v, path| match v {
                "" => Ok(None),
                _ => v
                    .parse()
                    .map_err(|err| Error::InvalidFloat { path: path(), err })
                    .map(Some),
            })
        };

        let latitude = opt_f64("latitude")??;
        let longitude = opt_f64("longitude")??;

        let coordinates = match (latitude, longitude) {
            (Some(latitude), Some(longitude)) => Some(Coordinates {
                latitude,
                longitude,
            }),
            _ => None,
        };

        let road = attribute_mapped(node, "pop", path, |v, path| match v {
            "R" => Ok(RoadKind::Regular),
            "A" => Ok(RoadKind::Highway),
            "N" => Ok(RoadKind::None),
            _ => Err(Error::InvalidRoadKind(path())),
        })??;

        Ok(Location {
            city,
            address: unwrap(address, "adresse")?,
            zip_code: attribute(node, "cp", path)?,
            coordinates,
            road,
        })
    }

    fn with_reader_format_hint<R: Read>(
        &self,
        mut reader: R,
        format: Option<DateFormat>,
    ) -> Parsed {
        let mut document = String::new();
        match self.re_encode {
            true => {
                let mut decoded = DecodeReaderBytesBuilder::new()
                    .encoding(Some(encoding_rs::WINDOWS_1252))
                    .build(reader);

                decoded.read_to_string(&mut document)?
            }
            false => reader.read_to_string(&mut document)?,
        };

        let date_format = self
            .date_format
            .or(format)
            .unwrap_or(DateFormat::TSeparated);

        let document = roxmltree::Document::parse(&document)?;

        let list = document
            .descendants()
            .find(|n| n.has_tag_name("pdv_liste"))
            .ok_or(Error::MissingNode {
                path: "".into(),
                node: "pdv_liste".into(),
            })?;

        let mut stations = Vec::new();

        for (idx, station) in list
            .children()
            .filter(|n| n.has_tag_name("pdv"))
            .enumerate()
        {
            let mut price_idx = 0;
            let mut shortage_idx = 0;
            let mut closing_idx = 0;

            let mut address = None;
            let mut city = None;
            let mut schedule = None;
            let mut services = Vec::new();
            let mut prices: EnumMap<_, Vec<_>> = EnumMap::default();
            let mut shortages: EnumMap<_, Vec<_>> = EnumMap::default();
            let mut closings = Vec::new();

            let path = || format!("pdv_liste.pdv[{idx}]");

            for child in station.children().filter(|n| n.is_element()) {
                match child.tag_name().name() {
                    "adresse" => address = Some(value(child, || path() + ".adresse")?),
                    "ville" => {
                        city = match value(child, || path() + ".ville") {
                            Ok(v) => Some(v),
                            Err(Error::MissingValue(_)) => None,
                            Err(e) => return Err(e),
                        }
                    }
                    "horaires" => {
                        schedule = Some(self.parse_schedule(child, || path() + ".horaires")?)
                    }
                    "services" => {
                        services = self.parse_services(child, || path() + ".services")?;
                    }
                    "prix" => {
                        if child.attributes().count() == 0 {
                            continue;
                        }

                        let (fuel, price) = self.parse_price(
                            child,
                            || path() + &format!(".prix[{price_idx}]"),
                            date_format,
                        )?;
                        price_idx += 1;
                        prices[fuel].push(price);
                    }
                    "rupture" => {
                        if child.attributes().count() == 0 {
                            continue;
                        }

                        let (fuel, shortage) = self.parse_shortage(
                            child,
                            || path() + &format!(".shortage[{shortage_idx}]"),
                            date_format,
                        )?;
                        shortage_idx += 1;
                        shortages[fuel].push(shortage);
                    }
                    "fermeture" => {
                        if child.attributes().count() == 0 {
                            continue;
                        }

                        closings.push(self.parse_closing(
                            child,
                            || path() + &format!(".fermeture[{closing_idx}]"),
                            date_format,
                        )?);
                        closing_idx += 1;
                    }
                    n if self.strict => {
                        return Err(Error::UnhandledNode {
                            path: path(),
                            node: n.into(),
                        })
                    }
                    _ => (),
                }
            }

            stations.push(Station {
                id: attribute(station, "id", path)?,
                location: self.parse_location(station, path, city, address)?,
                prices,
                shortages,
                services,
                schedule,
                closings,
            })
        }

        Ok(stations)
    }

    fn with_archive<R: Read + Seek>(
        &self,
        mut archive: ZipArchive<R>,
        date_format: Option<DateFormat>,
    ) -> Parsed {
        let file_name = archive.file_names().next().ok_or(Error::NoData)?.to_owned();
        let data = archive.by_name(&file_name)?;

        self.with_reader_format_hint(data, date_format)
    }

    async fn fetch(&self, url: &str, date_format: Option<DateFormat>) -> Parsed {
        log::debug!("fetching data at {url}");

        let body = reqwest::get(url).await?.bytes().await?;

        if body.starts_with(b"<!DOCTYPE html>") {
            let patterns = &[
                "Le fichier à cette date n’existe plus.".as_bytes(),
                "Les données à cette date ne sont pas disponible.".as_bytes(),
            ];

            let ac = AhoCorasick::new(patterns).unwrap();
            if ac.find(&body).is_some() {
                return Err(Error::NotFound);
            } else {
                panic!("Unexpected body: {:?}", std::str::from_utf8(&body));
            }
        }

        self.with_archive(ZipArchive::new(Cursor::new(body))?, date_format)
    }

    /// Fetch the instantaneous feed of data (-10 minutes).
    pub async fn fetch_instant(&self) -> Parsed {
        self.fetch(
            "https://donnees.roulez-eco.fr/opendata/instantane",
            Some(DateFormat::Spaced),
        )
        .await
    }

    /// Fetch the daily feed of data (updated at 05:00).
    pub async fn fetch_daily(&self) -> Parsed {
        self.fetch(
            "https://donnees.roulez-eco.fr/opendata/jour",
            Some(DateFormat::TSeparated),
        )
        .await
    }

    /// Fetch the data for the current year.
    pub async fn fetch_yearly(&self) -> Parsed {
        self.fetch(
            "https://donnees.roulez-eco.fr/opendata/annee",
            Some(DateFormat::TSeparated),
        )
        .await
    }

    /// Fetch the data for an archived year, only goes back to 2007.
    pub async fn fetch_archived_year(&self, year: u16) -> Parsed {
        self.fetch(
            &format!("https://donnees.roulez-eco.fr/opendata/annee/{year}"),
            Some(DateFormat::TSeparated),
        )
        .await
    }

    /// Fetch the data for a single archived day. Only the last 30 days are available.
    pub async fn fetch_archived_day(&self, year: u16, month: u8, day: u8) -> Parsed {
        self.fetch(
            &format!("https://donnees.roulez-eco.fr/opendata/jour/{year}{month:2}{day:2}"),
            Some(DateFormat::TSeparated),
        )
        .await
    }

    /// Directly open a zip file. [DateFormat::TSeparated] is assumed.
    pub fn with_zip<P: AsRef<Path>>(&self, path: P) -> Parsed {
        let file = File::open(path)?;
        self.with_archive(ZipArchive::new(file)?, None)
    }

    /// Directly open an XML file. [DateFormat::TSeparated] is assumed.
    pub fn with_file<P: AsRef<Path>>(&self, path: P) -> Parsed {
        self.with_reader(File::open(path)?)
    }

    /// Read data from a reader. [DateFormat::TSeparated] is assumed.
    pub fn with_reader<R: Read>(&self, reader: R) -> Parsed {
        self.with_reader_format_hint(reader, None)
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserBuilder {
    /// If false unknown keys don't return an [Error::UnhandledNode]. Defaults to true.
    pub fn strict(self, strict: bool) -> Self {
        Self { strict, ..self }
    }

    /// The XML file is usually encoded in Latin1, if false assume it's UTF-8 instead. Defaults to
    /// true.
    pub fn re_encode(self, re_encode: bool) -> Self {
        Self { re_encode, ..self }
    }

    /// Set the [DateFormat] to use in the parsed document.
    pub fn date_format(self, format: DateFormat) -> Self {
        Self {
            date_format: Some(format),
            ..self
        }
    }

    /// Create a [Parser] using the specified options.
    pub fn build(self) -> Parser {
        Parser {
            strict: self.strict,
            re_encode: self.re_encode,
            date_format: self.date_format,
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::VecDeque,
        sync::mpsc::{self, RecvTimeoutError, Sender},
        time::{Duration, Instant},
    };

    use chrono::Datelike;
    use once_cell::sync::Lazy;
    use test_log::test;

    use super::*;

    const DATA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../data");
    const RATE: Duration = Duration::from_millis(500);

    static LIMITER: Lazy<Sender<oneshot::Sender<()>>> = Lazy::new(|| {
        let (sender, recv) = mpsc::channel::<oneshot::Sender<()>>();

        std::thread::spawn(move || {
            let mut requests = VecDeque::new();
            let mut remaining_time = Duration::ZERO;

            loop {
                let before_sleep = Instant::now();

                match recv.recv_timeout(remaining_time) {
                    Ok(v) => requests.push_back(v),
                    Err(RecvTimeoutError::Timeout) => (),
                    Err(RecvTimeoutError::Disconnected) => panic!("Request thread died"),
                };

                if before_sleep.elapsed() >= remaining_time {
                    if let Some(request) = requests.pop_front() {
                        request.send(()).expect("Requester died");
                        remaining_time = RATE;
                    }
                } else {
                    remaining_time -= before_sleep.elapsed();
                }
            }
        });

        sender
    });

    fn get_token() {
        let (sender, recv) = oneshot::channel();

        LIMITER.send(sender).expect("limiter died");

        recv.recv().expect("limiter died")
    }

    #[test(tokio::test)]
    async fn fetch_instant() {
        get_token();

        if let Err(e) = Parser::new().fetch_instant().await {
            panic!("Error: {e:#?}")
        }
    }

    #[test(tokio::test)]
    async fn fetch_daily() {
        get_token();

        if let Err(e) = Parser::new().fetch_daily().await {
            panic!("Error: {e:#?}")
        }
    }

    #[test(tokio::test)]
    async fn fetch_archived_year() {
        get_token();

        if let Err(e) = Parser::new().fetch_archived_year(2020).await {
            panic!("Error: {e:#?}")
        }
    }

    #[test(tokio::test)]
    async fn fetch_archived_day() {
        get_token();

        let current_date = chrono::Utc::now();
        let archive_date = current_date - chrono::Days::new(10);

        if let Err(e) = Parser::new()
            .fetch_archived_day(
                archive_date.year() as u16,
                archive_date.month() as u8,
                archive_date.day() as u8,
            )
            .await
        {
            panic!("Error: {e:#?}")
        }
    }

    #[test(tokio::test)]
    #[ignore]
    async fn fetch_yearly() {
        get_token();

        if let Err(e) = Parser::new().fetch_yearly().await {
            panic!("Error: {e:#?}")
        }
    }

    #[test]
    fn zip() {
        let path = format!("{DATA_PATH}/PrixCarburants_quotidien_20231027.zip");

        if let Err(e) = Parser::new().with_zip(path) {
            panic!("Error: {e:#?}")
        };
    }

    #[test]
    fn file() {
        let path = format!("{DATA_PATH}/PrixCarburants_quotidien_20231026.xml");

        if let Err(e) = Parser::new().with_file(path) {
            panic!("Error: {e:#?}")
        };
    }

    #[test]
    fn day_2023_10_26() {
        let document = include_bytes!("../../data/PrixCarburants_quotidien_20231026.xml");

        if let Err(e) =
            Parser::new().with_reader_format_hint(&document[..], Some(DateFormat::TSeparated))
        {
            panic!("Error: {e:#?}");
        }
    }

    #[test]
    fn year_2023() {
        let document = include_bytes!("../../data/PrixCarburants_annuel_2023.xml");

        if let Err(e) =
            Parser::new().with_reader_format_hint(&document[..], Some(DateFormat::TSeparated))
        {
            panic!("Error: {e:#?}");
        }
    }

    // Missing a 'ville'
    #[test]
    fn year_2018() {
        let document = include_bytes!("../../data/PrixCarburants_annuel_2018.xml");

        if let Err(e) =
            Parser::new().with_reader_format_hint(&document[..], Some(DateFormat::TSeparated))
        {
            panic!("Error: {e:#?}");
        }
    }

    #[test]
    fn instant() {
        let document = include_bytes!("../../data/PrixCarburants_instantane.xml");

        if let Err(e) =
            Parser::new().with_reader_format_hint(&document[..], Some(DateFormat::Spaced))
        {
            panic!("Error: {e:#?}");
        }
    }

    #[test(tokio::test)]
    async fn year_too_old() {
        get_token();

        assert!(matches!(
            Parser::new().fetch_archived_year(2006).await,
            Err(Error::NotFound)
        ));
    }

    #[test(tokio::test)]
    async fn daily_too_old() {
        get_token();

        assert!(matches!(
            Parser::new().fetch_archived_day(2020, 10, 10).await,
            Err(Error::NotFound)
        ));
    }
}
