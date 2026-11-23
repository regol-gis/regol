use crate::{export, gol};
use regol::{DictionaryLoading, Library, source::Source};
use regol_overpass::{
    Counts, ExecutedGroup, GroupData, Request, area_index::AreaIndex, compile_request,
    execute_groups,
};
use std::{
    error::Error,
    ffi::OsString,
    fs,
    io::{self, BufWriter, Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

const USAGE: &str = "Usage:
  regol overpass FILE_OR_URL --file QUERY.ql [--bbox WEST,SOUTH,EAST,NORTH]
                 [-f count|ids|geojson] [--area-index DIRECTORY]
                 [--no-mmap] [--eager-dictionary] [--http-page-kib N]

Use --file - to read an Overpass QL query from stdin. --bbox supplies {{bbox}}.
Output is newline-delimited JSON with one record per out statement.
The default format is ids. Explicit out count groups always include counts.
--area-index reads a matching named-area index from disk.";

#[derive(Clone, Copy)]
enum Format {
    Count,
    Ids,
    GeoJson,
}

struct Options {
    gol: PathBuf,
    query: PathBuf,
    viewport: Option<Vec<[f64; 4]>>,
    format: Format,
    area_index: Option<PathBuf>,
    input: gol::Options,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Option<Self>, Box<dyn Error>> {
        let gol = args.next().ok_or("missing GOL file or URL")?;
        if gol == "-h" || gol == "--help" {
            return Ok(None);
        }
        let gol = PathBuf::from(gol);
        let mut query = None;
        let mut viewport = None;
        let mut format = Format::Ids;
        let mut area_index = None;
        let mut input = gol::Options::default();
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
                "--file" => query = Some(PathBuf::from(value()?)),
                "--bbox" => {
                    let value = value()?;
                    viewport = Some(bbox(value.to_str().ok_or("bbox must be UTF-8")?)?);
                }
                "--format" | "-f" => {
                    let value = value()?;
                    format = match value.to_str() {
                        Some("count") => Format::Count,
                        Some("ids") => Format::Ids,
                        Some("geojson") => Format::GeoJson,
                        _ => {
                            return Err(format!(
                                "unknown format {} (expected count, ids, or geojson)",
                                value.to_string_lossy()
                            )
                            .into());
                        }
                    };
                }
                "--area-index" => area_index = Some(PathBuf::from(value()?)),
                "--no-mmap" if inline.is_none() => input.mmap = false,
                "--mmap" if inline.is_none() => input.mmap = true,
                "--eager-dictionary" if inline.is_none() => {
                    input.dictionary = DictionaryLoading::Eager
                }
                "--lazy-dictionary" if inline.is_none() => {
                    input.dictionary = DictionaryLoading::OnDemand;
                }
                "--http-page-kib" => {
                    if gol::http_url(gol.as_os_str()).is_none() {
                        return Err("--http-page-kib requires an HTTP(S) URL".into());
                    }
                    input.set_http_page_kib(&value()?)?;
                }
                _ => return Err(format!("unknown option {arg}").into()),
            }
        }
        Ok(Some(Self {
            gol,
            query: query.ok_or("missing --file QUERY.ql (use --file - for stdin)")?,
            viewport,
            format,
            area_index,
            input,
        }))
    }

    fn request(&self) -> Result<Request, Box<dyn Error>> {
        let text = if self.query.as_os_str() == "-" {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("reading query from stdin: {error}"))?;
            text
        } else {
            fs::read_to_string(&self.query)
                .map_err(|error| format!("reading query {}: {error}", self.query.display()))?
        };
        Ok(compile_request(&text, self.viewport.as_deref())?)
    }

    fn index_path(&self, request: &mut Request) -> Result<Option<PathBuf>, Box<dyn Error>> {
        if let Some(path) = &self.area_index {
            return Ok(Some(path.clone()));
        }
        if request.named_areas_mut().next().is_none() {
            return Ok(None);
        }
        if gol::http_url(self.gol.as_os_str()).is_some() {
            return Err("named-area lookup for a GOL URL requires --area-index DIRECTORY".into());
        }
        let mut path = self.gol.as_os_str().to_os_string();
        path.push(".areas");
        let path = PathBuf::from(path);
        if path.try_exists()? {
            return Ok(Some(path));
        }
        request.allow_local_area_scan();
        Ok(None)
    }
}

fn bbox(text: &str) -> Result<Vec<[f64; 4]>, Box<dyn Error>> {
    let values: Vec<f64> = text
        .split(',')
        .map(|value| value.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| "bbox must contain numbers in west,south,east,north order")?;
    let [w, s, e, n] = values[..] else {
        return Err("bbox must have four values in west,south,east,north order".into());
    };
    if !(-180.0..=180.0).contains(&w)
        || !(-180.0..=180.0).contains(&e)
        || !(-90.0..=90.0).contains(&s)
        || !(-90.0..=90.0).contains(&n)
        || s > n
    {
        return Err("bbox coordinates are out of range or south exceeds north".into());
    }
    Ok(if w <= e {
        vec![[w, s, e, n]]
    } else {
        vec![[w, s, 180.0, n], [-180.0, s, e, n]]
    })
}

fn write_group<S: Source>(
    mut output: &mut impl Write,
    group: ExecutedGroup<'_, S>,
    format: Format,
    check: &impl Fn() -> Result<(), String>,
) -> Result<(), Box<dyn Error>> {
    check()?;
    output.write_all(b"{\"name\":")?;
    serde_json::to_writer(&mut output, &group.name)?;
    output.write_all(b",\"mode\":")?;
    serde_json::to_writer(&mut output, &group.mode)?;
    if let Some(detail) = group.detail {
        output.write_all(b",\"detail\":")?;
        serde_json::to_writer(&mut output, &detail)?;
    }
    if let Some(order) = group.order {
        output.write_all(b",\"order\":")?;
        serde_json::to_writer(&mut output, &order)?;
    }
    match group.data {
        GroupData::Count(counts) => {
            output.write_all(b",\"counts\":")?;
            serde_json::to_writer(&mut output, &counts)?;
        }
        GroupData::Features(features) => match format {
            Format::Count => {
                let mut counts = Counts::default();
                for feature in features {
                    check()?;
                    counts.add(feature.kind(), false);
                }
                output.write_all(b",\"counts\":")?;
                serde_json::to_writer(&mut output, &counts)?;
            }
            Format::Ids => {
                output.write_all(b",\"ids\":[")?;
                for (index, feature) in features.into_iter().enumerate() {
                    check()?;
                    if index != 0 {
                        output.write_all(b",")?;
                    }
                    write!(output, "\"{}\"", feature.id())?;
                }
                output.write_all(b"]")?;
            }
            Format::GeoJson => {
                output.write_all(b",\"type\":\"FeatureCollection\",\"features\":[")?;
                export::text::write_overpass_geojson(
                    features,
                    output,
                    group.mode,
                    group.detail,
                    check,
                )
                .map_err(|error| error as Box<dyn Error>)?;
                output.write_all(b"]")?;
            }
        },
    }
    check()?;
    output.write_all(b"}\n")?;
    Ok(())
}

fn run<S: Source>(
    library: &Library<S>,
    mut request: Request,
    area_index: Option<PathBuf>,
    format: Format,
    check: &impl Fn() -> Result<(), String>,
) -> Result<(), Box<dyn Error>> {
    check()?;
    if let Some(path) = area_index {
        AreaIndex::open(&path, library.source())?.hydrate(&mut request, check)?;
    }
    let groups = execute_groups(library, request, check)?;
    let mut output = BufWriter::with_capacity(128 * 1024, io::stdout().lock());
    for group in groups {
        write_group(&mut output, group, format, check)?;
    }
    output.flush()?;
    Ok(())
}

pub(super) fn execute(args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let Some(options) = Options::parse(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let mut request = options.request()?;
    let index = options.index_path(&mut request)?;
    let start = Instant::now();
    let timeout = request.timeout().map(Duration::from_secs);
    let check = || {
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            Err("Overpass query timed out.".into())
        } else {
            Ok(())
        }
    };
    match options.input.open(&options.gol)? {
        gol::Input::Http(library) => run(&library, request, index, options.format, &check),
        gol::Input::Mmap(library) => run(&library, request, index, options.format, &check),
        gol::Input::File(library) => run(&library, request, index, options.format, &check),
    }
}
