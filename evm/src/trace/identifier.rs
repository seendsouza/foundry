use ethers::{
    abi::{Abi, Address},
    prelude::ArtifactId,
};
use foundry_utils::diff_score;
use std::collections::BTreeMap;

/// Trace identifiers figure out what ABIs and labels belong to all the addresses of the trace.
pub trait TraceIdentifier {
    /// Attempts to identify an address in one or more call traces.
    ///
    /// The tuple is of the format `(contract, label, abi)`, where `contract` is intended to be of
    /// the format `"<artifact>:<contract>"`, e.g. `"Foo.json:Foo"`.
    fn identify_address(
        &self,
        address: &Address,
        code: Option<&Vec<u8>>,
    ) -> (Option<String>, Option<String>, Option<&Abi>);
}

/// The local trace identifier keeps track of addresses that are instances of local contracts.
pub struct LocalTraceIdentifier {
    local_contracts: BTreeMap<Vec<u8>, (String, Abi)>,
}

impl LocalTraceIdentifier {
    pub fn new(known_contracts: &BTreeMap<ArtifactId, (Abi, Vec<u8>)>) -> Self {
        Self {
            local_contracts: known_contracts
                .iter()
                .map(|(id, (abi, runtime_code))| {
                    (runtime_code.clone(), (id.name.clone(), abi.clone()))
                })
                .collect(),
        }
    }
}

impl TraceIdentifier for LocalTraceIdentifier {
    fn identify_address(
        &self,
        _: &Address,
        code: Option<&Vec<u8>>,
    ) -> (Option<String>, Option<String>, Option<&Abi>) {
        if let Some(code) = code {
            if let Some((_, (name, abi))) = self
                .local_contracts
                .iter()
                .find(|(known_code, _)| diff_score(known_code, code) < 0.1)
            {
                (Some(name.clone()), Some(name.clone()), Some(abi))
            } else {
                (None, None, None)
            }
        } else {
            (None, None, None)
        }
    }
}
