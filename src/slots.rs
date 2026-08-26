//! Four button-aligned cycling slot windows.
#![allow(dead_code)]

use std::sync::RwLock;

use crate::config::Config;

#[derive(Debug)]
pub struct Manager {
    inner: RwLock<Inner>,
}

#[derive(Debug)]
struct Inner {
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
        Manager {
            inner: RwLock::new(Inner { modules, indexes }),
        }
    }

    /// Apply a new configuration. Identical module lists preserve the current
    /// selection index; changed lists preserve the previously selected module
    /// when it still exists, otherwise select index 0.
    pub fn apply(&self, cfg: &Config) {
        let mut inner = self.inner.write().unwrap();
        for i in 0..4 {
            let next = cfg.slots[i].clone();
            if inner.modules[i] == next {
                inner.indexes[i] = normalized_index(inner.indexes[i] as i64, next.len());
                continue;
            }
            let old = inner.modules[i]
                .get(inner.indexes[i])
                .cloned()
                .unwrap_or_default();
            inner.modules[i] = next;
            inner.indexes[i] = inner.modules[i].iter().position(|m| *m == old).unwrap_or(0);
        }
    }

    pub fn current(&self, slot: usize) -> String {
        let inner = self.inner.read().unwrap();
        match inner.modules.get(slot) {
            Some(modules) if !modules.is_empty() => {
                modules[inner.indexes[slot] % modules.len()].clone()
            }
            _ => String::new(),
        }
    }

    pub fn cycle(&self, slot: usize, delta: i64) -> String {
        let mut inner = self.inner.write().unwrap();
        if slot >= 4 || inner.modules[slot].is_empty() {
            return String::new();
        }
        let n = inner.modules[slot].len() as i64;
        let mut index = (inner.indexes[slot] as i64 + delta) % n;
        if index < 0 {
            index += n;
        }
        inner.indexes[slot] = index as usize;
        inner.modules[slot][inner.indexes[slot]].clone()
    }

    pub fn indexes(&self) -> [usize; 4] {
        self.inner.read().unwrap().indexes
    }

    pub fn set_index(&self, slot: usize, index: i64) {
        let mut inner = self.inner.write().unwrap();
        if let Some(modules) = inner.modules.get(slot) {
            if !modules.is_empty() {
                inner.indexes[slot] = normalized_index(index, modules.len());
            }
        }
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
        let m = Manager::new(&Config::default(), [0; 4]);
        assert_eq!(m.cycle(0, 1), "CPU_TEMP");
        assert_eq!(m.cycle(0, 1), "CONTROLLER_BATTERY");
        assert_eq!(m.cycle(0, 1), "HEADSET_BATTERY", "wraps forward");
        assert_eq!(m.cycle(0, -1), "CONTROLLER_BATTERY", "wraps backward");
    }

    #[test]
    fn apply_preserves_selected_module_when_it_survives() {
        let c = cfg_with(1, &["FPS_CURRENT", "FPS_1LOW"]);
        let m = Manager::new(&c, [0; 4]);
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
        let m = Manager::new(&c, [0; 4]);
        m.cycle(2, 2);
        assert_eq!(m.current(2), "JITTER");
        m.apply(&Config::default());
        assert_eq!(m.current(2), "JITTER");
    }

    #[test]
    fn negative_initial_index_normalizes() {
        let m = Manager::new(&Config::default(), [-1, 0, 0, 0]);
        assert_eq!(m.current(0), "CONTROLLER_BATTERY");
    }
}
