// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

bitflags::bitflags! {
    pub struct WeekMode: u32 {
        const BEHAVIOR_MONDAY_FIRST  = 0b00000001;
        const BEHAVIOR_YEAR          = 0b00000010;
        const BEHAVIOR_FIRST_WEEKDAY = 0b00000100;
    }
}

impl WeekMode {
    #[must_use]
    pub fn to_normalized(self) -> WeekMode {
        let normalized = super::Time::normalize_week_mode_bits(self.bits());
        // XOR only the changed known bits, preserving the original bitflags
        // representation even if it carries otherwise unknown high bits.
        self ^ WeekMode::from_bits_truncate(self.bits() ^ normalized)
    }
}
