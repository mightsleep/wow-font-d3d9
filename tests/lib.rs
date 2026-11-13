#![allow(dead_code)]

#[path = "../src/atlas.rs"]
mod atlas;
#[path = "../src/coverage.rs"]
mod coverage;
#[path = "../src/glyph.rs"]
mod glyph;
#[path = "../src/ports.rs"]
mod ports;
#[path = "../src/record.rs"]
mod record;

#[cfg(test)]
mod reference;
#[cfg(test)]
mod shapes;

#[cfg(test)]
mod atlas_tests;
#[cfg(test)]
mod nonzero;
#[cfg(test)]
#[path = "ports.rs"]
mod ports_tests;
#[cfg(test)]
mod records;
