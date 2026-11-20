//! Reference winners: recent winning qblocks that the coordinator sends to
//! miners for calibration, and the hardware filter that selects them.

use crate::types::miner::MinerKind;

/// One recent winning qblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceWinner {
    /// Quantum-block id.
    pub qblock_id: u64,
    /// Winning miner account.
    pub miner: [u8; 32],
    /// Winning nonce, big-endian.
    pub nonce: [u8; 32],
    /// Energy the winner achieved, in milli units.
    pub energy_milli: i64,
    /// Energy ceiling of the block's difficulty, in milli units.
    pub max_energy_milli: i64,
    /// Topology the block was mined on.
    pub topology_hash: [u8; 32],
    /// Hardware the winner declared, or `None` when it has no record.
    pub kind: Option<MinerKind>,
}

/// One named hardware class a filter admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HardwareClass {
    Cpu,
    Gpu,
    Qpu,
}

impl HardwareClass {
    fn matches(self, kind: MinerKind) -> bool {
        match self {
            Self::Cpu => kind == MinerKind::Cpu,
            Self::Gpu => matches!(kind, MinerKind::Gpu | MinerKind::Metal),
            Self::Qpu => matches!(
                kind,
                MinerKind::QpuDwave | MinerKind::QpuIbm | MinerKind::QpuIonq | MinerKind::QpuPasqal
            ),
        }
    }
}

/// Which winners count as reference winners, by hardware. An empty filter
/// admits every winner, including one with an unknown kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HardwareFilter {
    classes: Vec<HardwareClass>,
}

impl HardwareFilter {
    /// Parse config strings: `"cpu"`, `"gpu"`, or `"qpu"`.
    ///
    /// # Errors
    /// Returns the offending string when it names no hardware class.
    pub fn parse<S: AsRef<str>>(kinds: &[S]) -> Result<Self, String> {
        let classes = kinds
            .iter()
            .map(|k| match k.as_ref() {
                "cpu" => Ok(HardwareClass::Cpu),
                "gpu" => Ok(HardwareClass::Gpu),
                "qpu" => Ok(HardwareClass::Qpu),
                other => Err(format!(
                    "unknown hardware kind {other:?}; expected cpu, gpu, or qpu"
                )),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { classes })
    }

    /// True when the filter has no entries, so it admits every winner.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Does the filter admit a winner of this kind? A non-empty filter
    /// rejects an unknown kind.
    #[must_use]
    pub fn admits(&self, kind: Option<MinerKind>) -> bool {
        if self.classes.is_empty() {
            return true;
        }
        kind.is_some_and(|k| self.classes.iter().any(|c| c.matches(k)))
    }
}
