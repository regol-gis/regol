#![no_main]
use regol::{Library, Query};
use libfuzzer_sys::fuzz_target;
mod buffered;

fuzz_target!(|text: &str| {
    if text.len() > 4096 {
        return;
    }
    if let Ok(query) = Query::parse(text) {
        for bytes in [
            include_bytes!("../../tests/fixtures/basic.gol").as_slice(),
            include_bytes!("../../tests/fixtures/query_semantics.gol").as_slice(),
        ] {
            let library = Library::new(bytes).unwrap();
            let simple: Vec<_> = library.select(&query).map(|f| f.unwrap().id()).collect();
            let prepared = query.prepare(&library).unwrap();
            let optimized: Vec<_> = prepared.iter().map(|f| f.unwrap().id()).collect();
            assert_eq!(simple, optimized);
            assert_eq!(prepared.count().unwrap(), simple.len() as u64);
            assert_eq!(
                prepared
                    .try_fold(Vec::new(), |mut ids, f| {
                        ids.push(f.id());
                        Ok(ids)
                    })
                    .unwrap(),
                simple
            );
            for width in [1, 31, 4096] {
                let buffered = Library::new(buffered::Buffered { bytes, width }).unwrap();
                let prepared = query.prepare(&buffered).unwrap();
                let actual: Vec<_> = prepared.iter().map(|f| f.unwrap().id()).collect();
                assert_eq!(simple, actual);
                assert_eq!(prepared.count().unwrap(), simple.len() as u64);
                assert_eq!(
                    prepared
                        .try_fold(Vec::new(), |mut ids, f| {
                            ids.push(f.id());
                            Ok(ids)
                        })
                        .unwrap(),
                    simple
                );
            }
        }
    }
});
