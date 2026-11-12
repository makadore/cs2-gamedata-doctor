//! gamedata-doctor: find the virtual tables of two builds of a stripped x86-64 binary through their RTTI,
//! match the slots by function fingerprints and report index shifts, then map gamedata offsets
//! from the old build to the new one.

pub mod diff;
pub mod elf;
pub mod fp;
pub mod fsutil;
pub mod gamedata;
pub mod image;
pub mod keep;
pub mod msvc;
pub mod pe;
pub mod report;
pub mod rtti;
pub mod sdk;
pub mod sdkcheck;
pub mod sdkfix;
pub mod sig;
pub mod uses;
