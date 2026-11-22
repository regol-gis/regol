//! Import tiles covering a bounding box from a local GOB into a new GOL.
//! Run with `cargo run --release --features gob --example import_gob -- planet.gob area.gol -0.178,51.482,-0.078,51.532`.
use regol::{
    Bounds, Latitude, LonLat, Longitude,
    gob::{GobCatalog, GobStore},
    units::angle::degree,
};
use std::{
    error::Error,
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
};

fn main() -> Result<(), Box<dyn Error>> {
    let [input, output, bbox] = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| "Usage: import_gob input.gob output.gol west,south,east,north")?;
    let coords: Vec<f64> = bbox.split(',').map(str::parse).collect::<Result<_, _>>()?;
    let [w, s, e, n] = coords
        .try_into()
        .map_err(|_| "Expected four bounding box coordinates")?;
    let point = |lon, lat| -> regol::Result<LonLat> {
        Ok(LonLat {
            longitude: Longitude::new::<degree>(lon)?,
            latitude: Latitude::new::<degree>(lat)?,
        })
    };
    let bounds = Bounds::from_lon_lat(point(w, s)?, point(e, n)?)?;
    let mut file = File::open(input)?;
    let mut prefix = vec![0; 64];
    file.read_exact(&mut prefix)?;
    prefix.resize(GobCatalog::prefix_len(&prefix)?, 0);
    file.read_exact(&mut prefix[64..])?;
    let catalog = GobCatalog::open(&prefix)?;
    if file.metadata()?.len() != catalog.archive_len() {
        return Err("File length differs from the archive catalog".into());
    }
    let tips = catalog.tiles_in_bounds(bounds);
    let mut store = GobStore::new(catalog, 256 * 1024 * 1024)?;
    for tip in tips {
        let tile = store
            .catalog()
            .tile(tip)
            .ok_or(regol::Error::MissingTile(tip))?;
        file.seek(SeekFrom::Start(tile.offset))?;
        let mut chunk = vec![0; tile.len as usize];
        file.read_exact(&mut chunk)?;
        store.import_tile(tip, &chunk)?;
    }
    let mut writer = BufWriter::new(File::create_new(output)?);
    store.write_gol(&mut writer)?;
    writer.flush()?;
    Ok(())
}
