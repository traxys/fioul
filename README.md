# fioul

Fioul is a set of programs to interact with French gas station informations.
The dataset is provided [by the government](https://www.prix-carburants.gouv.fr/rubrique/opendata/), as an XML file.

This repository contains a library ([fioul](./fioul)) to fetch and/or parse the data into memory.
It also provides a server ([fioul-server](./server)) to provide the data in an easier format.
A CLI tool ([fioul-cli](./cli)) is provided to query the server easily.

## Server

The server provides a single HTTP route that returns data as a JSON document: `/api/stations`

The server caches the government data, only fetching it once every 5 minutes for instantaneous data and once every 30 minutes for all other sources.

It also caches failure to get the data for 1 hour.

The response is of the form:

```json
{
    "status": "ok",
    "stations": [],
}
```

Or in case of an error:

```json
{
    "status": "error",
    "code": 0,
    "message": "<some error specifc message>"
}
```

There are a number of query parameters that can be supplied:

- `source`: One of `instant`, `current_day`, `current_year`, `historic_day`, `historic_year`.
  This parameter allows to specify the source of data. Some variants require additional parameters.
- `year`: Specify the year for `historic_day` or  `historic_year`
- `month`: Specify the month for `historic_month`
- `day`: Specify the day for `historic_day`
- `location`: Takes a value of the form `latitude,longitude,distance` (with distance in meters) and
  returns only the stations closer than `distance` of the `latitude,longitude`.
- `location_keep_unknown`: One of `true` or `false`. Some stations don't have geographic coordinates.
  Those are excluded from a `location` search by default, but if this parameter is true they will be
  included.
- `ids`: Comma separated list of integer ids of stations, allows to query only specific stations.

## CLI

The CLI can query a fioul server for information. It can also use [nominatim](https://nominatim.org/) 
to fetch extra information.

It is recommended to use a self-hosted nominatim instance, as doing `fioul near` can generate a
lot of queries, that will not be compliant with nominatim usage requirements (even if they are
cached once done).

The CLI can be configured by placing a file in `~/.config/fioul/config.toml`.
An [example configuration](./config.toml) is provided, with all the possible options.
