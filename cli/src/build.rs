use regol::build::{BuildOptions, TileZooms, build_pbf_with_layout};
use std::{error::Error, ffi::OsString, path::PathBuf};

const USAGE: &str = "Usage:
  regol build [OPTIONS] INPUT OUTPUT

Compile an OpenStreetMap PBF snapshot (.osm.pbf) into a GOL 2.0 file.

Options:
  --preset NAME                Apply a layout preset (default, vector-tiles, compact)
  --compact-dictionary         Keep only required keys and frequent strings in the global table
  --even-zooms                 Use 4x4 even zoom steps (0/2/4/6/8/10/12)
  --target-tile-kib N          Target tile size in KiB (default 16384)
  --max-duplicate-tiles K      Max leaf tiles per border-straddling feature (default 1)
  --rtree-fanout F             R-tree branch fanout (2..=255, default 8)
  --rtree-leaf-size L          R-tree leaf capacity (default 32)
  --memory-limit-mib N         Memory budget in MiB before spilling to disk (default: auto)";

pub(super) fn execute(mut args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let _command = args.next();
    let mut options = BuildOptions::default();
    let mut zooms = TileZooms::default();
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut after_double_dash = false;

    while let Some(arg) = args.next() {
        if !after_double_dash {
            if arg == "--" {
                after_double_dash = true;
                continue;
            }
            if arg == "--help" || arg == "-h" {
                println!("{USAGE}");
                return Ok(());
            }
            if let Some(s) = arg.to_str()
                && s.starts_with('-')
            {
                let (flag, inline) = s
                    .split_once('=')
                    .map_or((s, None), |(flag, value)| (flag, Some(value)));
                let mut value = || -> Result<String, Box<dyn Error>> {
                    let raw = inline
                        .map(OsString::from)
                        .or_else(|| args.next())
                        .ok_or_else(|| format!("missing value for {flag}"))?;
                    raw.into_string()
                        .map_err(|_| format!("invalid UTF-8 value for {flag}").into())
                };
                match flag {
                    "--preset" => match value()?.as_str() {
                        "default" => {
                            options = BuildOptions::default();
                            zooms = TileZooms::default();
                        }
                        "vector-tiles" => {
                            // Tuned for high-latency range reads / vector-tile extraction:
                            // 4 MiB tiles, 4x4 even zooms, border duplication up to 16 tiles,
                            // and wider R-tree nodes (fanout 16, leaf 64) to minimize round trips.
                            options = BuildOptions {
                                target_tile_bytes: 4096 * 1024,
                                max_duplicate_tiles: 16,
                                rtree_branch_fanout: 16,
                                rtree_leaf_size: 64,
                                ..BuildOptions::default()
                            };
                            zooms = TileZooms::Even;
                        }
                        "compact" => {
                            options = BuildOptions {
                                compact_dictionary: true,
                                ..BuildOptions::default()
                            };
                            zooms = TileZooms::Even;
                        }
                        other => {
                            return Err(format!(
                                    "unknown --preset {other} (expected default, vector-tiles, or compact)"
                                )
                                .into());
                        }
                    },
                    "--compact-dictionary" if inline.is_none() => {
                        options.compact_dictionary = true;
                    }
                    "--even-zooms" if inline.is_none() => {
                        zooms = TileZooms::Even;
                    }
                    "--target-tile-kib" => {
                        let kib: u64 = value()?.parse().map_err(|_| "invalid --target-tile-kib")?;
                        if kib == 0 {
                            return Err("--target-tile-kib must be greater than 0".into());
                        }
                        options.target_tile_bytes =
                            kib.checked_mul(1024).ok_or("--target-tile-kib overflow")?;
                    }
                    "--max-duplicate-tiles" => {
                        let k: usize = value()?
                            .parse()
                            .map_err(|_| "invalid --max-duplicate-tiles")?;
                        if k == 0 {
                            return Err("--max-duplicate-tiles must be at least 1".into());
                        }
                        options.max_duplicate_tiles = k;
                    }
                    "--rtree-fanout" => {
                        options.rtree_branch_fanout =
                            value()?.parse().map_err(|_| "invalid --rtree-fanout")?;
                    }
                    "--rtree-leaf-size" => {
                        options.rtree_leaf_size =
                            value()?.parse().map_err(|_| "invalid --rtree-leaf-size")?;
                    }
                    "--memory-limit-mib" => {
                        let mib: u64 =
                            value()?.parse().map_err(|_| "invalid --memory-limit-mib")?;
                        options.memory_limit_bytes = Some(
                            mib.checked_mul(1024 * 1024)
                                .ok_or("--memory-limit-mib overflow")?,
                        );
                    }
                    _ => return Err(format!("unknown option {s}").into()),
                }
                continue;
            }
        }
        positional.push(arg.into());
    }

    let [input, output] = positional.as_slice() else {
        return Err("usage: regol build [OPTIONS] INPUT OUTPUT".into());
    };
    let report =
        build_pbf_with_layout(input, output, options, zooms).map_err(|err| format!("{err:#}"))?;
    eprintln!(
        "Wrote {} features in {} tiles. Omitted {} degenerate or incomplete ways, {} unlocated relations and {} unavailable members.",
        report.features,
        report.tiles,
        report.omitted_ways,
        report.omitted_relations,
        report.omitted_members
    );
    Ok(())
}
