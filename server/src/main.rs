use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
    time::Instant,
};

use axum::{
    extract::{rejection::QueryRejection, FromRequestParts, Query},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use fioul::Station;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::{
    mpsc::{self, UnboundedSender},
    oneshot, RwLock,
};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

type AppState = axum::extract::State<Arc<State>>;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
enum Source {
    Instant,
    CurrentDay,
    CurrentYear,
    HistoricDay,
    HistoricYear,
}

impl Default for Source {
    fn default() -> Self {
        Self::Instant
    }
}

#[derive(Debug, Clone, Copy)]
enum PreciseSource {
    Instant,
    CurrentDay,
    CurrentYear,
    HistoricDay { year: u16, month: u8, day: u8 },
    HistoricYear(u16),
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("missing 'year' query parameter")]
    MissingYear,
    #[error("{0}")]
    QueryRejection(#[from] QueryRejection),
    #[error("connection error to the upstream service")]
    Upstream,
    #[error("missing 'day' query parameter")]
    MissingDay,
    #[error("missing 'month' query parameter")]
    MissingMonth,
    #[error("upstream data should not have been missing")]
    UnexpectedMissingUpstreamData,
    #[error("data does not exist upstream")]
    NotFound,
}

impl Error {
    fn error_info(&self) -> (u64, StatusCode) {
        match self {
            Error::MissingYear => (0, StatusCode::BAD_REQUEST),
            Error::QueryRejection(_) => (1, StatusCode::BAD_REQUEST),
            Error::Upstream => (2, StatusCode::SERVICE_UNAVAILABLE),
            Error::MissingDay => (3, StatusCode::BAD_REQUEST),
            Error::MissingMonth => (4, StatusCode::BAD_REQUEST),
            Error::UnexpectedMissingUpstreamData => (5, StatusCode::SERVICE_UNAVAILABLE),
            Error::NotFound => (6, StatusCode::NOT_FOUND),
        }
    }
}

#[derive(Serialize, Deserialize)]
enum ResponseKind {
    Error,
    Ok,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status")]
#[serde(rename_all = "snake_case")]
enum Response<T> {
    Error { code: u64, message: String },
    Ok(T),
}

struct OkResponse<T>(T);

impl<T> From<T> for OkResponse<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> IntoResponse for Response<T>
where
    T: Serialize,
{
    fn into_response(self) -> axum::response::Response {
        Json(self).into_response()
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let (code, status) = self.error_info();

        (
            status,
            Response::<()>::Error {
                code,
                message: self.to_string(),
            },
        )
            .into_response()
    }
}

impl<T> IntoResponse for OkResponse<T>
where
    T: Serialize,
{
    fn into_response(self) -> axum::response::Response {
        Response::Ok(self.0).into_response()
    }
}

struct QueryJE<T>(T);

#[axum::async_trait]
impl<T, S> FromRequestParts<S> for QueryJE<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(v) = Query::from_request_parts(parts, state).await?;
        Ok(Self(v))
    }
}

impl PreciseSource {
    pub fn from_source(
        source: Source,
        year: Option<u16>,
        month: Option<u8>,
        day: Option<u8>,
    ) -> Result<Self, Error> {
        match source {
            Source::Instant => Ok(Self::Instant),
            Source::CurrentDay => Ok(Self::CurrentDay),
            Source::CurrentYear => Ok(Self::CurrentYear),
            Source::HistoricDay => Ok(Self::HistoricDay {
                year: year.ok_or(Error::MissingYear)?,
                month: month.ok_or(Error::MissingMonth)?,
                day: day.ok_or(Error::MissingDay)?,
            }),
            Source::HistoricYear => Ok(Self::HistoricYear(year.ok_or(Error::MissingYear)?)),
        }
    }
}

#[derive(Deserialize, Debug)]
struct QueryParams {
    #[serde(default)]
    source: Source,
    year: Option<u16>,
    month: Option<u8>,
    day: Option<u8>,
}

#[derive(Serialize)]
struct Stations {
    stations: Vec<Station>,
}

async fn stations(
    state: AppState,
    Query(params): Query<QueryParams>,
) -> Result<OkResponse<Stations>, Error> {
    let source = PreciseSource::from_source(params.source, params.year, params.month, params.day)?;

    let stations = state.data.get_data(source).await?;

    Ok(Stations { stations }.into())
}

struct DataEntry {
    constructed: Instant,
    data: Vec<Station>,
}

enum Entry {
    Data(DataEntry),
    NotFound(Instant),
}

struct QueryCacheInner {
    years: HashMap<u16, Entry>,
    days: HashMap<(u16, u8, u8), Entry>,
    current_day: Option<DataEntry>,
    current_year: Option<DataEntry>,
    instant: Option<DataEntry>,
}

struct QueryCache {
    data: RwLock<QueryCacheInner>,

    limiter: UnboundedSender<oneshot::Sender<()>>,

    instant_duration: Duration,
    cache_duration: Duration,
    not_found_timeout: Duration,
}

impl Entry {
    fn expect_found(self) -> Result<DataEntry, Error> {
        match self {
            Entry::Data(d) => Ok(d),
            Entry::NotFound(_) => Err(Error::UnexpectedMissingUpstreamData),
        }
    }

    /// None will trigger a re-fetch, and Some will be passed along
    fn read(&self, not_found_timeout: Duration) -> Option<Result<&DataEntry, Error>> {
        match self {
            Entry::Data(d) => Some(Ok(d)),
            Entry::NotFound(d) => {
                if d.elapsed() > not_found_timeout {
                    tracing::debug!("Error expired, re-fetching");
                    None
                } else {
                    Some(Err(Error::NotFound))
                }
            }
        }
    }
}

impl QueryCacheInner {
    fn read(
        &self,
        source: PreciseSource,
        not_found_timeout: Duration,
    ) -> Option<Result<&DataEntry, Error>> {
        match source {
            PreciseSource::Instant => self.instant.as_ref().map(Ok),
            PreciseSource::CurrentDay => self.current_day.as_ref().map(Ok),
            PreciseSource::CurrentYear => self.current_year.as_ref().map(Ok),
            PreciseSource::HistoricDay { year, month, day } => {
                self.days.get(&(year, month, day))?.read(not_found_timeout)
            }
            PreciseSource::HistoricYear(year) => self.years.get(&year)?.read(not_found_timeout),
        }
    }

    fn replace(&mut self, source: PreciseSource, data: Entry) -> Result<(), Error> {
        match source {
            PreciseSource::Instant => self.instant = Some(data.expect_found()?),
            PreciseSource::CurrentDay => self.current_day = Some(data.expect_found()?),
            PreciseSource::CurrentYear => self.current_year = Some(data.expect_found()?),
            PreciseSource::HistoricDay { year, month, day } => {
                self.days.insert((year, month, day), data);
            }
            PreciseSource::HistoricYear(year) => {
                self.years.insert(year, data);
            }
        }

        Ok(())
    }
}

const RATE_LIMIT: Duration = Duration::from_millis(500);

impl QueryCache {
    #[tracing::instrument(skip(self))]
    async fn get_data(&self, source: PreciseSource) -> Result<Vec<Station>, Error> {
        let duration = match source {
            PreciseSource::Instant => self.instant_duration,
            _ => self.cache_duration,
        };

        let handle = self.data.read().await;

        match handle.read(source, self.not_found_timeout) {
            Some(Ok(d)) if d.constructed.elapsed() < duration => Ok(d.data.clone()),
            Some(Err(e)) => Err(e),
            d => {
                if d.is_some() {
                    tracing::debug!("data expired, re-fetching")
                }

                drop(handle);

                // We could have been slower than someone else when taking the write lock, so we
                // need to check again if someone else fetched the data
                let mut write_handle = self.data.write().await;
                match write_handle.read(source, self.not_found_timeout) {
                    // Check again for elapsed in case of any strange behaviour / system lag
                    Some(Ok(d)) if d.constructed.elapsed() < duration => Ok(d.data.clone()),
                    Some(Err(e)) => Err(e),
                    _ => {
                        let data = self.fetch_data(source).await?;

                        write_handle.replace(
                            source,
                            match data.clone() {
                                Some(data) => Entry::Data(DataEntry {
                                    constructed: Instant::now(),
                                    data,
                                }),
                                None => Entry::NotFound(Instant::now()),
                            },
                        )?;

                        data.ok_or(Error::NotFound)
                    }
                }
            }
        }
    }

    async fn fetch_data(&self, source: PreciseSource) -> Result<Option<Vec<Station>>, Error> {
        let parser = self.get_token().await;

        let fetched = match source {
            PreciseSource::Instant => parser.fetch_instant().await,
            PreciseSource::CurrentDay => parser.fetch_daily().await,
            PreciseSource::CurrentYear => parser.fetch_yearly().await,
            PreciseSource::HistoricDay { year, month, day } => {
                parser.fetch_archived_day(year, month, day).await
            }
            PreciseSource::HistoricYear(year) => parser.fetch_archived_year(year).await,
        };

        match fetched {
            Ok(d) => Ok(Some(d)),
            Err(e) => match e {
                fioul::Error::NotFound => Ok(None),
                e => {
                    tracing::error!("Could not fetch from upstream: {e:?}");
                    Err(Error::Upstream)
                }
            },
        }
    }

    async fn get_token(&self) -> fioul::Parser {
        let (sender, recv) = oneshot::channel();

        self.limiter.send(sender).expect("limiter died");

        recv.await.expect("limiter dropped our request");

        fioul::Parser::new()
    }

    fn new() -> Self {
        let (limiter, mut recv) = mpsc::unbounded_channel::<oneshot::Sender<()>>();

        tokio::spawn(async move {
            let mut requests = VecDeque::new();
            let mut remaining_time = Duration::ZERO;

            loop {
                let before_sleep = Instant::now();

                match tokio::time::timeout(remaining_time, recv.recv()).await {
                    Ok(Some(v)) => requests.push_back(v),
                    Ok(None) => return,
                    Err(_) => (),
                }

                if before_sleep.elapsed() >= remaining_time {
                    loop {
                        if let Some(request) = requests.pop_front() {
                            if request.send(()).is_err() {
                                continue;
                            }

                            remaining_time = RATE_LIMIT;
                        }

                        break;
                    }
                } else {
                    remaining_time -= before_sleep.elapsed();
                }
            }
        });

        Self {
            data: QueryCacheInner {
                years: HashMap::new(),
                days: HashMap::new(),
                current_day: None,
                current_year: None,
                instant: None,
            }
            .into(),

            limiter,
            instant_duration: Duration::from_secs(60 * 5),
            cache_duration: Duration::from_secs(60 * 30),
            not_found_timeout: Duration::from_secs(60 * 60),
        }
    }
}

struct State {
    data: QueryCache,
}

impl State {
    pub fn new() -> Self {
        Self {
            data: QueryCache::new(),
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = Arc::new(State::new());

    let app = Router::new()
        .route("/api/stations", get(stations))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::new("0.0.0.0".parse().unwrap(), 3000);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}
