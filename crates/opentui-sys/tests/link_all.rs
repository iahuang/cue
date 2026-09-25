//! References every generated declaration, so linking fails if the bindings
//! name a symbol that `libopentui` does not export.

#[test]
fn every_symbol_links() {
    include!(concat!(env!("OUT_DIR"), "/link_test.rs"));
}
