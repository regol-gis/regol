//! Warm, in-process kernels. Output is observational, not a CI timing gate.
use regol::{FeatureType, Library, Query, Result};
use std::{hint::black_box, time::Instant};

fn measure(name: &str, iterations: u32, mut f: impl FnMut() -> Result<u64>) -> Result<()> {
    let start = Instant::now();
    let mut work = 0;
    for _ in 0..iterations {
        work += black_box(f()?);
    }
    let elapsed = start.elapsed().as_nanos();
    println!(
        "{name},{iterations},{},{work}",
        elapsed / u128::from(iterations)
    );
    Ok(())
}
fn main() -> Result<()> {
    let bytes = std::fs::read(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "tests/fixtures/stress.gol".to_owned()),
    )?;
    let iterations = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let lib = Library::new(bytes.as_slice())?;
    let query = Query::parse("n[amenity=cafe][capacity>=500]")?;
    let prepared = query.prepare(&lib)?;
    println!("kernel,iterations,ns_per_iteration,work_units");
    measure("open", iterations, || {
        Ok(black_box(Library::new(bytes.as_slice())?).source().len() as u64)
    })?;
    measure("first_result", iterations, || {
        Ok(lib.features().next().transpose()?.map_or(0, |f| f.id().id))
    })?;
    measure("scan", iterations, || {
        lib.features().try_fold(0, |n, f| {
            black_box(f?);
            Ok(n + 1)
        })
    })?;
    measure("query", iterations, || {
        lib.select(&query).try_fold(0, |n, f| {
            black_box(f?);
            Ok(n + 1)
        })
    })?;
    measure("prepared_query", iterations, || {
        prepared.iter().try_fold(0, |n, f| {
            black_box(f?);
            Ok(n + 1)
        })
    })?;
    measure("tags", iterations, || {
        let mut count = 0;
        for f in lib.features() {
            for t in f?.tags()? {
                black_box(t?);
                count += 1;
            }
        }
        Ok(count)
    })?;
    measure("coordinates", iterations, || {
        let mut count = 0;
        for f in lib.features() {
            let f = f?;
            if f.kind() == FeatureType::Way {
                for p in f.coordinates()? {
                    black_box(p?);
                    count += 1;
                }
            }
        }
        Ok(count)
    })?;
    measure("members", iterations, || {
        let mut count = 0;
        for f in lib.features() {
            let f = f?;
            if f.kind() == FeatureType::Relation {
                for m in f.members()? {
                    black_box(m?);
                    count += 1;
                }
            }
        }
        Ok(count)
    })?;
    measure("owned_geometry", iterations, || {
        let mut count = 0;
        for f in lib.features() {
            black_box(f?.geometry()?);
            count += 1;
        }
        Ok(count)
    })?;
    Ok(())
}
