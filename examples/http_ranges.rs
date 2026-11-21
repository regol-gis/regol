//! Measure a bounded query with an empty source cache and again on the same handle.
use regol::{
    Bounds, DictionaryLoading, Latitude, Library, Limits, LonLat, Longitude, Query, QueryExecutor,
    source::{HttpSource, PagedFile, Source},
    units::angle::degree,
};
use std::{env, error::Error, fs::File, time::Instant};

fn measure<S: Source + Sync>(
    start: Instant,
    source: S,
    query: &Query,
    bounds: Bounds,
    threads: usize,
    operation: &str,
    stats: impl Fn(&S) -> (u64, u64, Option<&'static str>),
) -> Result<(), Box<dyn Error>> {
    let lib =
        Library::with_dictionary_loading(source, Limits::default(), DictionaryLoading::OnDemand)?;
    let prepared = query.prepare(&lib)?;
    let execute = || -> regol::Result<u64> {
        if operation == "count" {
            return QueryExecutor::count_once(threads, &prepared, Some(bounds));
        }
        let pool = QueryExecutor::new(threads)?;
        match operation {
            "fold" => pool.try_fold_reduce(
                &prepared,
                Some(bounds),
                || 0,
                |n, _| Ok(n + 1),
                |a, b| Ok(a + b),
            ),
            "stream" => pool.with_query(&prepared, Some(bounds), |items| {
                items.try_fold(0, |n, item| item.map(|_| n + 1))
            })?,
            "first" => pool.with_query(&prepared, Some(bounds), |items| {
                items.next().transpose().map(|f| u64::from(f.is_some()))
            })?,
            _ => unreachable!("validated benchmark operation"),
        }
    };
    let count = execute()?;
    let cold = start.elapsed();
    let (gets, bytes, _) = stats(lib.source());
    let start = Instant::now();
    let repeat = execute()?;
    let warm = start.elapsed();
    assert_eq!(repeat, count);
    let (total_gets, total_bytes, protocol) = stats(lib.source());
    if let Some(protocol) = protocol {
        eprintln!("protocol={protocol}");
    }
    println!("count,cold_ms,warm_ms,cold_gets,cold_bytes,warm_gets,warm_bytes");
    println!(
        "{count},{:.3},{:.3},{gets},{bytes},{},{}",
        cold.as_secs_f64() * 1e3,
        warm.as_secs_f64() * 1e3,
        total_gets - gets,
        total_bytes - bytes
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [file, kib, selector, bbox, extra @ ..] = args.as_slice() else {
        return Err(
            "usage: http_ranges FILE_OR_URL PAGE_KIB|auto GOQL WEST,SOUTH,EAST,NORTH [THREADS [count|fold|stream|first]]"
                .into(),
        );
    };
    let (threads, operation) = match extra {
        [] => (1, "count"),
        [n] => (n.parse()?, "count"),
        [n, mode] if matches!(mode.as_str(), "count" | "fold" | "stream" | "first") => {
            (n.parse()?, mode.as_str())
        }
        _ => {
            return Err("expected a thread count and optional count, fold, stream or first".into());
        }
    };
    if threads == 0 {
        return Err("threads must be positive".into());
    }
    let values = bbox
        .split(',')
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()?;
    let &[west, south, east, north] = values.as_slice() else {
        return Err("bbox needs four coordinates".into());
    };
    let corner = |lon, lat| -> regol::Result<LonLat> {
        Ok(LonLat {
            longitude: Longitude::new::<degree>(lon)?,
            latitude: Latitude::new::<degree>(lat)?,
        })
    };
    let bounds = Bounds::from_lon_lat(corner(west, south)?, corner(east, north)?)?;
    let query = Query::parse(selector)?;
    let start = Instant::now();
    if file.starts_with("http://") || file.starts_with("https://") {
        let source = if kib == "auto" {
            HttpSource::open(file)?
        } else {
            let size = kib
                .parse::<usize>()?
                .checked_mul(1024)
                .ok_or("page size overflow")?;
            HttpSource::with_page_size(file, size)?
        };
        eprintln!("page_kib={}", source.page_bytes() / 1024);
        measure(start, source, &query, bounds, threads, operation, |s| {
            (s.cached_pages(), s.cached_bytes(), Some(s.protocol()))
        })
    } else {
        measure(
            start,
            PagedFile::from_file_sparse(File::open(file)?)?,
            &query,
            bounds,
            threads,
            operation,
            |_| (0, 0, None),
        )
    }
}
