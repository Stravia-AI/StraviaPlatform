//! 固定份额是可借用的保护目标，不是各类独立预算或用户配置。

use std::cmp::Reverse;

pub(super) const CLASS_COUNT: usize = 7;
pub(super) const TOTAL_WEIGHT: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum CacheClass {
    Metadata,
    Allowance,
    CredentialMappings,
    Materialized,
    Catalog,
    Affinity,
    Opportunistic,
}

impl CacheClass {
    pub(super) const ALL: [Self; CLASS_COUNT] = [
        Self::Metadata,
        Self::Allowance,
        Self::CredentialMappings,
        Self::Materialized,
        Self::Catalog,
        Self::Affinity,
        Self::Opportunistic,
    ];

    pub(super) fn for_domain(domain: &str) -> Self {
        match domain {
            "pricing" | "credential_mapping_presence" => Self::Metadata,
            "provider_allowance" => Self::Allowance,
            "credential_mappings" => Self::CredentialMappings,
            "generation.materialized" => Self::Materialized,
            "generation.catalog" => Self::Catalog,
            "affinity" => Self::Affinity,
            _ => Self::Opportunistic,
        }
    }

    pub(super) const fn index(self) -> usize {
        self as usize
    }

    pub(super) const fn priority(self) -> u8 {
        match self {
            Self::Metadata | Self::Allowance => 0,
            Self::CredentialMappings | Self::Materialized => 1,
            Self::Catalog | Self::Affinity => 2,
            Self::Opportunistic => 3,
        }
    }

    pub(super) const fn weight(self) -> usize {
        match self {
            Self::Metadata => 1,
            Self::Allowance => 3,
            Self::CredentialMappings => 6,
            Self::Materialized => 16,
            Self::Catalog => 2,
            Self::Affinity => 4,
            Self::Opportunistic => 0,
        }
    }

    pub(super) const fn quota(self, capacity: usize) -> usize {
        // 先除后乘避免大预算溢出；取整余量继续作为所有类别的可借空间。
        capacity / TOTAL_WEIGHT * self.weight()
            + (capacity % TOTAL_WEIGHT) * self.weight() / TOTAL_WEIGHT
    }
}

fn deficit(capacity: usize, usage: &[usize; CLASS_COUNT], weight: usize) -> Option<usize> {
    let used = usage
        .iter()
        .try_fold(0_usize, |sum, used| sum.checked_add(*used))?;
    let free = capacity.checked_sub(used)?;
    (weight <= capacity).then(|| weight.saturating_sub(free))
}

/// usage 不含待替换的同键旧值；拒绝写入前不破坏仍然有效的旧值。
pub(super) fn can_admit(
    capacity: usize,
    usage: &[usize; CLASS_COUNT],
    incoming: CacheClass,
    weight: usize,
) -> bool {
    let Some(needed) = deficit(capacity, usage, weight) else {
        return false;
    };
    if needed == 0 {
        return true;
    }
    let mut lower_excess = 0;
    let mut higher_excess = 0;
    for class in CacheClass::ALL {
        if class == incoming {
            continue;
        }
        let excess = usage[class.index()].saturating_sub(class.quota(capacity));
        if class.priority() >= incoming.priority() {
            lower_excess += excess;
        } else {
            higher_excess += excess;
        }
    }
    // 自身可换出旧条目；跨类只回收借用量。高优先级借用者也须归还份额。
    needed <= usage[incoming.index()] + lower_excess + higher_excess.min(incoming.quota(capacity))
}

/// 确定类别后，由后端原生队列选取条目；整条淘汰允许越过保护目标。
pub(super) fn victim_class(
    capacity: usize,
    usage: &[usize; CLASS_COUNT],
    incoming: CacheClass,
    weight: usize,
) -> Option<CacheClass> {
    let needed = deficit(capacity, usage, weight)?;
    if needed == 0 {
        return None;
    }
    let reclaim_headroom = incoming
        .quota(capacity)
        .saturating_sub(usage[incoming.index()]);
    CacheClass::ALL
        .into_iter()
        .filter(|class| {
            usage[class.index()] > class.quota(capacity)
                && (class.priority() >= incoming.priority() || needed <= reclaim_headroom)
        })
        .max_by_key(|class| {
            (
                class.priority(),
                usage[class.index()] - class.quota(capacity),
                Reverse(class.index()),
            )
        })
        .or_else(|| (usage[incoming.index()] > 0).then_some(incoming))
}
