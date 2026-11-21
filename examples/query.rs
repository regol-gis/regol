use regol::{Library, Query, source::PagedFile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: query FILE.gol [GOQL]")?;
    let query = Query::parse(&args.next().unwrap_or_else(|| "*".to_owned()))?;
    let library = Library::new(PagedFile::open(path)?)?;
    let prepared = query.prepare(&library)?;
    for feature in prepared.iter() {
        let feature = feature?;
        print!("{}", feature.id());
        for tag in feature.tags()? {
            let tag = tag?;
            print!("\t{}={}", tag.key, tag.value);
        }
        println!();
    }
    Ok(())
}
