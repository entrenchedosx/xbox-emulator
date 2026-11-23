use std::collections::HashMap;
use std::sync::Mutex;
use tombox_xshader::hlsl::ShaderRegister;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    pub mip_uavs: Vec<ShaderRegister>,
    pub mip_srvs: Vec<ShaderRegister>,
    pub gmem: bool,
}

impl Needs {
    pub fn of(t: &tombox_xshader::hlsl::Translated) -> Needs {
        Needs { mip_uavs: t.mip_uavs.clone(), mip_srvs: t.mip_srvs.clone(), gmem: t.needs_gmem }
    }

    pub fn any(&self) -> bool {
        !self.mip_uavs.is_empty() || !self.mip_srvs.is_empty() || self.gmem
    }

    pub fn refusal(&self) -> Option<String> {
        if !self.any() {
            return None;
        }
        let slot = |s: &ShaderRegister| format!("space{} r{}", s.space, s.register);
        let mut parts = Vec::new();
        if !self.mip_uavs.is_empty() {
            parts.push(format!("per-mip UAV views of {}", self.mip_uavs.iter().map(slot).collect::<Vec<_>>().join(", ")));
        }
        if !self.mip_srvs.is_empty() {
            parts.push(format!("per-mip SRV views of {}", self.mip_srvs.iter().map(slot).collect::<Vec<_>>().join(", ")));
        }
        if self.gmem {
            parts.push("a global-memory (BVH node) buffer".to_string());
        }
        Some(format!(
            "shader needs {}, which d3d12_xs does not bind yet (no root-signature table in space {} / {} and no descriptor ring in the title's heap)",
            parts.join(" and "),
            tombox_xshader::hlsl::TB_MIP_SPACE,
            tombox_xshader::hlsl::TB_GMEM_SPACE
        ))
    }
}

static SEEN: Mutex<Option<HashMap<u64, Needs>>> = Mutex::new(None);

pub fn note(hash: u64, n: &Needs) {
    if n.any() {
        SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(hash, n.clone());
    }
}

pub fn lookup(hash: u64) -> Option<Needs> {
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&hash).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_are_recorded_and_refused_with_the_slots() {
        let none = Needs::default();
        assert_eq!(none.refusal(), None);
        note(1, &none);
        assert_eq!(lookup(1), None);
        let n = Needs { mip_uavs: vec![ShaderRegister { space: 2, register: 3 }], mip_srvs: vec![], gmem: false };
        note(7, &n);
        assert_eq!(lookup(7), Some(n.clone()));
        let r = n.refusal().unwrap();
        assert!(r.contains("per-mip UAV views of space2 r3"), "{r}");
        let g = Needs { gmem: true, ..Default::default() };
        assert!(g.refusal().unwrap().contains("global-memory"));
    }
}
