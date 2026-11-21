use regol::{
    Library,
    source::{CachedFile, HttpSource, Source},
};
use std::{
    error::Error,
    ffi::OsString,
    io::{self, Write},
};

pub(super) fn execute(mut args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let check = args.next().is_some_and(|command| command == "check");
    let name = if check { "check" } else { "info" };
    let file = args
        .next()
        .ok_or_else(|| format!("usage: regol {name} FILE_OR_URL"))?;
    if args.next().is_some() {
        return Err(format!("usage: regol {name} FILE_OR_URL").into());
    }
    if file == "--help" || file == "-h" {
        println!("Usage: regol {name} FILE_OR_URL");
        return Ok(());
    }
    if let Some(url) = super::http_url(&file) {
        inspect(Library::new(HttpSource::open(url)?)?, check)
    } else {
        inspect(Library::new(CachedFile::open(file, 16 << 20)?)?, check)
    }
}

fn inspect<S: Source>(library: Library<S>, check: bool) -> Result<(), Box<dyn Error>> {
    let mut output = io::BufWriter::new(io::stdout().lock());
    if check {
        let report = library.check_integrity()?;
        writeln!(
            output,
            "Verified tiles: {}. Features: {}.",
            report.tiles, report.features
        )?;
    } else {
        writeln!(output, "Format: GOL 2")?;
        writeln!(output, "Bytes: {}", library.source().len())?;
        writeln!(output, "Active snapshot: {}", library.active_snapshot())?;
        writeln!(output, "Revision: {}", library.revision()?)?;
        writeln!(output, "Tiles: {}", library.tile_count())?;
        writeln!(output, "Way node IDs: {}", library.has_way_node_ids())?;
        let keys = library.indexed_keys().collect::<regol::Result<Vec<_>>>()?;
        writeln!(output, "Indexed keys: {}", keys.join(", "))?;
        for property in library.properties()? {
            let (key, value) = property?;
            writeln!(output, "{key}: {value}")?;
        }
    }
    output.flush()?;
    Ok(())
}
