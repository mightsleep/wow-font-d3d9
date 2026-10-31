#![allow(dead_code)]

#[path = "../src/glyph.rs"]
mod glyph;
#[path = "../src/atlas.rs"]
mod atlas;
#[path = "../src/coverage.rs"]
mod coverage;
#[path = "../src/record.rs"]
mod record;
#[path = "../src/ports.rs"]
mod ports;

#[cfg(test)]
mod reference;
#[cfg(test)]
mod shapes;

#[cfg(test)]
mod nonzero;
#[cfg(test)]
mod records;
#[cfg(test)]
mod atlas_tests;
#[cfg(test)]
#[path = "ports.rs"]
mod ports_tests;
