//! Four button-aligned cycling slot windows.
#![allow(dead_code)]

use crate::config::Config;

#[derive(Debug)]
pub struct Manager {
    modules: [Vec<String>; 4],
    indexes: [usize; 4],
}

fn normalized_index(index: i64, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let count = count as i64;
    let mut index = index % count;
    if index < 0 {
        index += count;
    }
    index as usize
}

impl Manager {
    pub fn new(cfg: &Config, initial_indexes: [i64; 4]) -> Self {
        let modules = [
            cfg.slots[0].clone(),
            cfg.slots[1].clone(),
            cfg.slots[2].clone(),
            cfg.slots[3].clone(),
        ];
        let indexes = [
            normalized_index(initial_indexes[0], modules[0].len()),
            normalized_index(initial_indexes[1], modules[1].len()),
            normalized_index(initial_indexes[2], modules[2].len()),
            normalized_index(initial_indexes[3], modules[3].len()),
        ];
        Manager { modules, indexes }
    }

    /// Apply a new configuration. Identical module lists preserve the current
    /// selection index; changed lists preserve the previously selected module
    /// when it still exists, otherwise select index 0.
    pub fn apply(&mut self, cfg: &Config) {
        for i in 0..4 {
            let next = cfg.slots[i].clone();
            if self.modules[i] == next {
                self.indexes[i] = normalized_index(self.indexes[i] as i64, next.len());
                continue;
            }
            let old = self.modules[i]
                .get(self.indexes[i])
                .cloned()
                .unwrap_or_default();
            self.modules[i] = next;
            self.indexes[i] = self.modules[i].iter().position(|m| *m == old).unwrap_or(0);
        }
    }

    pub fn current(&self, slot: usize) -> String {
        match self.modules.get(slot) {
            Some(modules) if !modules.is_empty() => {
                modules[self.indexes[slot] % modules.len()].clone()
            }
            _ => String::new(),
        }
    }

    pub fn cycle(&mut self, slot: usize, delta: i64) -> String {
        if slot >= 4 || self.modules[slot].is_empty() {
            return String::new();
        }
        let n = self.modules[slot].len() as i64;
        let mut index = (self.indexes[slot] as i64 + delta) % n;
        if index < 0 {
            index += n;
        }
        self.indexes[slot] = index as usize;
        self.modules[slot][self.indexes[slot]].clone()
    }

    /// Return the next configured module not in `excluded`, without changing selection.
    pub fn next_except(&self, slot: usize, excluded: &[&str]) -> Option<String> {
        let modules = self.modules.get(slot)?;
        (1..=modules.len())
            .map(|offset| &modules[(self.indexes[slot] + offset) % modules.len()])
            .find(|module| {
                !excluded
                    .iter()
                    .any(|item| module.eq_ignore_ascii_case(item))
            })
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg_with(slot: usize, modules: &[&str]) -> Config {
        let mut c = Config::default();
        c.slots[slot] = modules.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn cycle_wraps_forward_and_backward() {
        let mut m = Manager::new(&Config::default(), [0; 4]);
        assert_eq!(m.cycle(0, 1), "CPU_TEMP");
        assert_eq!(m.cycle(0, 1), "CONTROLLER_BATTERY");
        assert_eq!(m.cycle(0, 1), "HEADSET_BATTERY", "wraps forward");
        assert_eq!(m.cycle(0, -1), "CONTROLLER_BATTERY", "wraps backward");
    }

    #[test]
    fn apply_preserves_selected_module_when_it_survives() {
        let c = cfg_with(1, &["FPS_CURRENT", "FPS_1LOW"]);
        let mut m = Manager::new(&c, [0; 4]);
        m.cycle(1, 1);
        assert_eq!(m.current(1), "FPS_1LOW");
        m.apply(&cfg_with(1, &["FPS_CURRENT", "FPS_1LOW", "FRAME_TIME"]));
        assert_eq!(m.current(1), "FPS_1LOW", "selection preserved on grow");
        m.apply(&cfg_with(1, &["FRAME_TIME", "FPS_1LOW"]));
        assert_eq!(m.current(1), "FPS_1LOW", "selection follows module");
        m.apply(&cfg_with(1, &["RAM_USAGE"]));
        assert_eq!(m.current(1), "RAM_USAGE", "falls back to index 0");
    }

    #[test]
    fn identical_list_preserves_index() {
        let c = Config::default();
        let mut m = Manager::new(&c, [0; 4]);
        m.cycle(2, 2);
        assert_eq!(m.current(2), "DISK_IO");
        m.apply(&Config::default());
        assert_eq!(m.current(2), "DISK_IO");
    }

    #[test]
    fn negative_initial_index_normalizes() {
        let m = Manager::new(&Config::default(), [-1, 0, 0, 0]);
        assert_eq!(m.current(0), "CONTROLLER_BATTERY");
    }

    #[test]
    fn dynamic_default_falls_through_without_mutating_selection() {
        let manager = Manager::new(&Config::default(), [0; 4]);
        assert_eq!(manager.current(2), "PROC_HANG");
        assert_eq!(
            manager.next_except(2, &["PROC_HANG", "BOTTLENECK"]),
            Some("FPS_1LOW".into())
        );
        assert_eq!(manager.current(2), "PROC_HANG");
        let manager = Manager::new(&cfg_with(0, &["PROC_HANG", "BOTTLENECK"]), [0; 4]);
        assert_eq!(manager.next_except(0, &["PROC_HANG", "BOTTLENECK"]), None);
    }
}
