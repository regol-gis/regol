mod area;
mod build;
mod export;
mod inspect;

use area::Area;
use regol::{
    Bounds, DictionaryLoading, Feature, Library, Limits, Query, QueryExecutor, source::Source,
};
use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    io::{self, BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
    thread,
};

const USAGE: &str = "Usage:
  regol build [OPTIONS] INPUT OUTPUT
  regol query FILE_OR_URL GOQL [--area GEOJSON] [--bbox WEST,SOUTH,EAST,NORTH]
              [-f count|ids|xml|pbf|geojson|geojsonl|wkt|csv]
              [--keys KEYS] [--columns COLUMNS] [--precision DIGITS]
              [--complete-relations TYPES] [--threads N] [--unordered|--ordered]
              [--no-mmap] [--eager-dictionary] [--http-page-kib N]
  regol info FILE_OR_URL
  regol check FILE_OR_URL

Build compiles an OpenStreetMap .osm.pbf file into a GOL 2.0 file; run
`regol build --help` for layout and tuning options.
Query writes to stdout. The default format is count. The bounding box selects
features by their stored bounds. A GeoJSON area selects features by geometry
intersection.
--keys selects comma-separated GeoJSON properties or CSV columns.
--columns also selects CSV columns (default id,type,geometry).
--precision sets the number of coordinate decimal places (0–15, default 7).
Use -R or --complete-relations to specify which relation types to complete in
spatial PBF exports. Accepts all, none, or comma-separated types (default multipolygon).
Use -j or --threads to set the CPU budget for counts and parallel exports (default: available CPUs).
--threads 1 keeps encoding and PBF compression on the calling thread.
--unordered (default for multi-threaded GeoJSON, GeoJSONL, WKT, CSV, and ID
exports) scans and encodes spatial partitions in parallel tile order.
Use --ordered (or --threads 1) to preserve serial query order.
HTTP(S) URLs use cached byte-range GETs. --http-page-kib sets the range size
for URL queries (power of two, 4–16384 KiB). By default the first 128 KiB
transfer determines the range size for that source.
By default, query memory-maps immutable local GOL files and loads the dictionary on demand.
Use --no-mmap for positioned file reads and --eager-dictionary to validate the full
string table on open (--mmap and --lazy-dictionary are also accepted).";

enum Format {
    Count,
    Ids,
    Export(export::Format),
    Text(export::text::Format),
}

struct Options {
    file: PathBuf,
    query: Query,
    area: Option<PathBuf>,
    bounds: Option<Bounds>,
    format: Format,
    text: export::text::Options,
    completion: export::Completion,
    mmap: bool,
    http_page_bytes: Option<usize>,
    dictionary: DictionaryLoading,
    threads: Option<usize>,
    unordered: bool,
}

impl Options {
    fn query_threads(&self) -> usize {
        self.threads.unwrap_or_else(|| {
            thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        })
    }

    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Option<Self>, Box<dyn Error>> {
        let Some(command) = args.next() else {
            return Err("missing command".into());
        };
        if command == "--help" || command == "-h" {
            return Ok(None);
        }
        if command != "query" {
            return Err(format!("unknown command {}", command.to_string_lossy()).into());
        }
        let file = args.next().ok_or("missing GOL file")?;
        if file == "--help" || file == "-h" {
            return Ok(None);
        }
        let query = args.next().ok_or("missing GOQL query")?;
        let query = query.to_str().ok_or("GOQL query must be UTF-8")?;
        let mut options = Self {
            file: file.into(),
            query: Query::parse(query)?,
            area: None,
            bounds: None,
            format: Format::Count,
            text: export::text::Options::default(),
            completion: export::Completion::default(),
            mmap: true,
            http_page_bytes: None,
            dictionary: DictionaryLoading::OnDemand,
            threads: None,
            unordered: true,
        };
        while let Some(arg) = args.next() {
            let arg = arg.to_str().ok_or("option must be UTF-8")?;
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(flag, value)| (flag, Some(value)));
            let mut value = || {
                inline
                    .map(OsString::from)
                    .or_else(|| args.next())
                    .ok_or_else(|| format!("missing value for {flag}"))
            };
            match flag {
                "--help" | "-h" if inline.is_none() => return Ok(None),
                "--area" | "-a" => options.area = Some(value()?.into()),
                "--bbox" | "-b" => {
                    let value = value()?;
                    options.bounds = Some(area::parse_bbox(
                        value.to_str().ok_or("bbox must be UTF-8")?,
                    )?);
                }
                "--format" | "-f" => {
                    let value = value()?;
                    options.format = match value.to_str() {
                        Some("count") => Format::Count,
                        Some("list" | "ids") => Format::Ids,
                        Some("xml") => Format::Export(export::Format::Xml),
                        Some("pbf") => Format::Export(export::Format::Pbf),
                        Some("geojson") => Format::Text(export::text::Format::GeoJson),
                        Some("geojsonl") => Format::Text(export::text::Format::GeoJsonLines),
                        Some("wkt") => Format::Text(export::text::Format::Wkt),
                        Some("csv") => Format::Text(export::text::Format::Csv),
                        _ => {
                            return Err(
                                format!("unknown format {}", value.to_string_lossy()).into()
                            );
                        }
                    };
                }
                "--complete-relations" | "-R" => {
                    let value = value()?;
                    options.completion = value
                        .to_str()
                        .ok_or("relation types must be UTF-8")?
                        .parse()?;
                }
                "--keys" | "-k" => {
                    let value = value()?;
                    options
                        .text
                        .set_keys(value.to_str().ok_or("tag keys must be UTF-8")?)?;
                }
                "--columns" => {
                    let value = value()?;
                    options
                        .text
                        .set_columns(value.to_str().ok_or("column names must be UTF-8")?)?;
                }
                "--precision" | "-p" => {
                    let value = value()?;
                    options
                        .text
                        .set_precision(value.to_str().ok_or("precision must be UTF-8")?)?;
                }
                "--lazy-dictionary" if inline.is_none() => {
                    options.dictionary = DictionaryLoading::OnDemand
                }
                "--eager-dictionary" if inline.is_none() => {
                    options.dictionary = DictionaryLoading::Eager
                }
                "--mmap" if inline.is_none() => options.mmap = true,
                "--no-mmap" if inline.is_none() => options.mmap = false,
                "--http-page-kib" => {
                    if http_url(options.file.as_os_str()).is_none() {
                        return Err("--http-page-kib requires an HTTP(S) URL".into());
                    }
                    let bytes = value()?
                        .to_str()
                        .and_then(|s| s.parse::<usize>().ok())
                        .and_then(|n| n.checked_mul(1024))
                        .ok_or("--http-page-kib requires a size in KiB")?;
                    options.http_page_bytes = Some(bytes);
                }
                "--threads" | "-j" => {
                    let value = value()?;
                    let threads: usize = value
                        .to_str()
                        .ok_or("thread count must be UTF-8")?
                        .parse()
                        .map_err(|_| "threads must be a positive integer")?;
                    if threads == 0 {
                        return Err("threads must be at least 1".into());
                    }
                    options.threads = Some(threads);
                }
                "--unordered" if inline.is_none() => options.unordered = true,
                "--ordered" if inline.is_none() => options.unordered = false,
                _ => return Err(format!("unknown option {arg}").into()),
            }
        }
        options.text.validate(match options.format {
            Format::Text(format) => Some(format),
            _ => None,
        })?;
        Ok(Some(options))
    }
}

fn select<'a, S: Source>(
    feature: regol::Result<Feature<'a, S>>,
    area: Option<&Area>,
) -> Option<regol::Result<Feature<'a, S>>> {
    let (Ok(feature), Some(area)) = (&feature, area) else {
        return Some(feature);
    };
    match area.intersects_feature(*feature) {
        Ok(true) => Some(Ok(*feature)),
        Ok(false) => None,
        Err(error) => Some(Err(error)),
    }
}

fn run<S: Source + Sync>(
    library: &Library<S>,
    options: &Options,
    area: Option<&Area>,
    output: &mut (impl Write + Send),
) -> Result<(), Box<dyn Error>> {
    let prepared = options.query.prepare(library)?;
    let bounds = options.bounds.or_else(|| area.map(Area::bounds));
    if matches!(options.format, Format::Count) {
        let threads = options.query_threads();
        let count = match (area, bounds) {
            (Some(area), Some(bounds)) if threads > 1 && library.tile_count() > 1 => {
                QueryExecutor::try_fold_reduce_once(
                    threads,
                    &prepared,
                    Some(bounds),
                    4096,
                    || 0u64,
                    |count, feature| Ok(count + u64::from(area.intersects_feature(feature)?)),
                    |left, right| Ok(left + right),
                )?
            }
            (Some(area), Some(bounds)) => {
                prepared.try_fold_in_bounds(bounds, 0, |count, feature| {
                    Ok(count + u64::from(area.intersects_feature(feature)?))
                })?
            }
            _ => QueryExecutor::count_once(threads, &prepared, bounds)?,
        };
        writeln!(output, "{count}")?;
    } else if options.unordered
        && options.query_threads() > 1
        && library.tile_count() > 1
        && matches!(options.format, Format::Ids | Format::Text(_))
    {
        let threads = options.query_threads();
        match options.format {
            Format::Ids => {
                export::text::write_ids_partitioned(&prepared, bounds, area, output, threads)
                    .map_err(|error| error as Box<dyn Error>)?;
            }
            Format::Text(format) => {
                export::text::write_partitioned(
                    &prepared,
                    bounds,
                    area,
                    output,
                    format,
                    &options.text,
                    threads,
                )
                .map_err(|error| error as Box<dyn Error>)?;
            }
            Format::Count | Format::Export(_) => unreachable!(),
        }
    } else {
        // Both public iterators draw from the same query plan. A world search
        // would also decode and validate bounds for otherwise unbounded queries.
        let candidates = match bounds {
            Some(bounds) => Candidates::Left(prepared.in_bounds(bounds)),
            None => Candidates::Right(prepared.iter()),
        };
        let features = candidates.filter_map(|feature| select(feature, area));
        match options.format {
            Format::Ids => {
                let mut digits = itoa::Buffer::new();
                for feature in features {
                    let id = feature?.id();
                    output.write_all(&[export::id_prefix(id.kind)])?;
                    output.write_all(digits.format(id.id).as_bytes())?;
                    output.write_all(b"\n")?;
                }
            }
            Format::Export(format) => {
                let within_area = |feature: Feature<'_, S>| {
                    // Relation members come directly from their parent and
                    // bypass the query's bounds filter.
                    if let Some(bounds) = bounds
                        && !bounds.intersects(feature.bounds()?)
                    {
                        return Ok(false);
                    }
                    area.map_or(Ok(true), |area| area.intersects_feature(feature))
                };
                let scope = bounds.is_some().then_some(&within_area as &dyn Fn(_) -> _);
                export::export(
                    features,
                    output,
                    format,
                    library.has_way_node_ids(),
                    &options.completion,
                    scope,
                    options.threads,
                )?
            }
            Format::Text(format) => {
                export::text::write_parallel(
                    features,
                    output,
                    format,
                    &options.text,
                    options.query_threads(),
                )
                .map_err(|error| error as Box<dyn Error>)?;
            }
            Format::Count => unreachable!(),
        }
    }
    Ok(())
}

enum Candidates<L, R> {
    Left(L),
    Right(R),
}

impl<T, L: Iterator<Item = T>, R: Iterator<Item = T>> Iterator for Candidates<L, R> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self {
            Self::Left(left) => left.next(),
            Self::Right(right) => right.next(),
        }
    }
}

fn http_url(file: &OsStr) -> Option<&str> {
    let value = file.to_str()?;
    let (scheme, _) = value.split_once("://")?;
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")).then_some(value)
}

fn execute() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1).peekable();
    if args.len() == 1
        && args
            .peek()
            .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("regol {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.peek().is_some_and(|arg| arg == "build") {
        return build::execute(args);
    }
    if args
        .peek()
        .is_some_and(|arg| arg == "check" || arg == "info")
    {
        return inspect::execute(args);
    }
    let Some(options) = Options::parse(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let area = options.area.as_deref().map(Area::open).transpose()?;
    let mut output = BufWriter::with_capacity(128 * 1024, io::stdout());
    if let Some(url) = http_url(options.file.as_os_str()) {
        let source = match options.http_page_bytes {
            Some(size) => regol::source::HttpSource::with_page_size(url, size)?,
            None => regol::source::HttpSource::open(url)?,
        };
        run(
            &Library::with_dictionary_loading(source, Limits::default(), options.dictionary)?,
            &options,
            area.as_ref(),
            &mut output,
        )?;
    } else if options.mmap {
        // SAFETY: `regol query` requires the GOL file to remain immutable for
        // the lifetime of the command unless `--no-mmap` is specified.
        let source = unsafe { regol::source::MappedFile::open(&options.file)? };
        run(
            &Library::with_dictionary_loading(source, Limits::default(), options.dictionary)?,
            &options,
            area.as_ref(),
            &mut output,
        )?;
    } else {
        run(
            &Library::open_with_dictionary_loading(&options.file, options.dictionary)?,
            &options,
            area.as_ref(),
            &mut output,
        )?;
    }
    output.flush()?;
    Ok(())
}

fn main() -> ExitCode {
    match execute() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("regol: {error}");
            ExitCode::FAILURE
        }
    }
}
