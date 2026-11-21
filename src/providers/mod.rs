//! Telemetry providers: Windows-local clock, per-logical-processor CPU load,
//! memory load, and native CCD topology detection.

pub mod audio;
pub mod ccd;
pub mod clock;
pub mod connections;
pub mod cpu;
pub mod discord;
pub mod gpu;
pub mod hang;
pub mod headset;
pub mod hwinfo;
pub mod lhm;
pub mod memory;
pub mod netif;
pub mod network;
pub mod performance;
pub mod presentmon;
pub mod system_power;
pub mod xinput;

#[cfg(test)]
mod tests {
    /// EC-8 (P1 §2 EC-8, acceptance 3): the shipped checklist summary in
    /// `docs/ARCHITECTURE.md` must name every provider module declared here, one table row
    /// each, so the re-arm checklist cannot drift from the tree (EC-5 docs-pin precedent:
    /// `providers/cpu.rs::ec5_docs_name_the_single_derivation_and_family`).
    #[test]
    fn ec8_docs_checklist_names_every_provider() {
        const DOCS: &str = include_str!("../../docs/ARCHITECTURE.md");
        const DECLARATIONS: &str = include_str!("mod.rs");
        let heading = "## Provider config re-arm checklist (EC-8)";
        let start = DOCS
            .find(heading)
            .expect("docs must carry the EC-8 provider checklist");
        let table = &DOCS[start..];
        let declared: Vec<&str> = DECLARATIONS
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("pub mod ")
                    .and_then(|name| name.strip_suffix(';'))
            })
            .collect();
        assert!(
            declared.len() >= 18,
            "the provider index must declare every module: {declared:?}"
        );
        for module in &declared {
            let token = format!("| `src/providers/{module}.rs`");
            assert!(
                table.contains(&token),
                "EC-8 checklist must name {token} in a table row"
            );
        }
        let rows = table
            .lines()
            .filter(|line| line.starts_with("| `src/providers/"))
            .count();
        assert_eq!(
            rows,
            declared.len(),
            "EC-8 checklist needs exactly one row per provider module"
        );
    }
}
