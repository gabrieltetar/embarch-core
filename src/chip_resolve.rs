//! Zephyr SoC name → probe-rs chip target string (decision 8).
//!
//! `embarch-api`'s Zephyr/west live target discovery (its own `embarch-api`
//! decision 12) knows a project's SoC name (e.g. `nrf54l15`, read straight out
//! of a board's directory layout) but has no reason to carry its own copy of
//! the SoC→probe-rs-chip table — Core already links `probe-rs` as a library
//! (`hardware.rs`) and is about to be asked to `/flash`/`/reset` with
//! whatever this resolves to anyway, so this is the one place that can both
//! hold the mapping and check it against the real thing.
//!
//! Deliberately not just a hardcoded string return: `resolve` looks the
//! mapped candidate up in probe-rs's own built-in target registry before
//! handing it back, so a stale table entry (probe-rs renames or drops a
//! target in some future version) fails exactly like an unmapped SoC would,
//! rather than silently succeeding here and only blowing up later at
//! `/flash` time with a confusing "failed to attach to target" error.

use probe_rs::config::Registry;

/// Table covers this suite's real Nordic hardware family (the nRF54L15DK
/// `embarch-dev-bench` hosts on, plus its siblings) rather than every chip
/// probe-rs knows about — extend when a real repo's board scan turns up a
/// SoC name not listed here. This `const` is the **only** place a mapping
/// can be added: it is compiled in, and there is no config file — in this
/// crate or in `embarch-api` — that can override or extend it, so a new SoC
/// costs a source edit, a rebuild and a redeploy. Keys are Zephyr SoC names,
/// lowercased, as they appear in a board's directory/`board.yml` (e.g.
/// `soc/nordic/nrf54l/`); values are the exact probe-rs target name
/// `get_target_by_name` expects.
const SOC_TO_CHIP: &[(&str, &str)] = &[
    ("nrf51822", "nRF51822_xxAA"),
    ("nrf52805", "nRF52805_xxAA"),
    ("nrf52810", "nRF52810_xxAA"),
    ("nrf52811", "nRF52811_xxAA"),
    ("nrf52820", "nRF52820_xxAA"),
    ("nrf52832", "nRF52832_xxAA"),
    ("nrf52833", "nRF52833_xxAA"),
    ("nrf52840", "nRF52840_xxAA"),
    ("nrf5340", "nRF5340_xxAA"),
    ("nrf9151", "nRF9151_xxAA"),
    ("nrf9160", "nRF9160_xxAA"),
    ("nrf9161", "nRF9161_xxAA"),
    ("nrf54l15", "nRF54L15"),
    ("nrf54lm20a", "nRF54LM20A"),
    // ESP32-C5: interim substitute dev-bench board while the real nRF54L15DK
    // is RMA'd (`embarch-dev-bench` decision 26). probe-rs's own target name is lowercase
    // `esp32c5`, unlike the Nordic entries' `nRF*` casing above — this table
    // preserves each target's own real probe-rs spelling rather than
    // normalizing a convention across vendors.
    ("esp32c5", "esp32c5"),
    // First non-Nordic, non-Espressif entry, and the first one where the
    // Zephyr key is a *series* name rather than a part number. `board.yml`
    // for a G0 board names the SoC `stm32g0b1xx` — Zephyr's own
    // `soc/st/stm32/stm32g0x/` naming, where the trailing `xx` stands in for
    // both the package letter and the flash-size letter — but probe-rs wants
    // one concrete variant (`chip-list stm32g0b1` offers sixteen, CB through
    // VE). So this row is lossy in a way none of the rows above are, and the
    // loss is not cosmetic: within G0B1 the final letter *is* the flash size
    // (B = 128 KiB, C = 256 KiB, E = 512 KiB), which is what probe-rs's
    // memory map and flash algorithm are built from.
    //
    // What the loss does and does not cost was measured against probe-rs's
    // own registry rather than reasoned about, because the first repo to need
    // this row (chargerito) builds for two different G0B1 packages through
    // this one key — its custom `chargerito_core` board (STM32G0B1**VE**, per
    // its `board.cmake` J-Link `--device`) and upstream Zephyr's
    // `nucleo_g0b1re` bring-up board (STM32G0B1**RE**). Dumping
    // `Target::memory_map` for both:
    //
    //   STM32G0B1VE == STM32G0B1RE  BANK_1 0x08000000..0x08040000 (256 KiB)
    //                               BANK_2 0x08040000..0x08080000 (256 KiB)
    //                               SRAM   0x20000000 + 144 KiB
    //   STM32G0B1CB                 BANK_1 0x08000000..0x08020000 (128 KiB)
    //                               SRAM   0x20000000 + 144 KiB   (no BANK_2)
    //
    // So the **flash-size letter is the axis that matters and the package
    // letter is not** — VE and RE are byte-identical here, and it is the
    // 128 KiB `B` part that has a genuinely different map (single bank).
    // One row therefore covers every `stm32g0b1x**E**` board correctly, which
    // is both boards in play.
    //
    // Where it would break: a `stm32g0b1xB`/`xC` board would get 512 KiB
    // dual-bank declared for a 128/256 KiB single-bank part. That
    // over-declares rather than corrupts — a write past the real end fails
    // loudly — but it is still wrong, and at that point the key stops being
    // expressible as one row: the table would need a real (soc, flash_size)
    // key rather than a longer comment.
    ("stm32g0b1xx", "STM32G0B1VE"),
];

/// The SoC named didn't resolve — either it's not in `SOC_TO_CHIP` at all, or
/// the table's entry no longer matches a real probe-rs target. Either way
/// the caller's next step is the same manual fallback — `embarch-core
/// chip-list` for the real target name, then a `SOC_TO_CHIP` edit and a
/// rebuild — so both cases collapse into one error rather than being
/// distinguished — a caller can't act differently on "unmapped" vs.
/// "mapped but stale" anyway.
#[derive(Debug)]
pub struct UnmappedSoc(pub String);

impl std::fmt::Display for UnmappedSoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no known probe-rs chip mapping for SoC '{}' — run `embarch-core chip-list <part>` for the right probe-rs target name, then add the SoC/target pair to `SOC_TO_CHIP` in embarch-core's `chip_resolve.rs` and rebuild. The table is compiled in; there is no config file that can override it",
            self.0
        )
    }
}

impl std::error::Error for UnmappedSoc {}

/// Resolve a Zephyr SoC name to a probe-rs chip target string, validated
/// against probe-rs's real built-in target registry (not just this table).
///
/// Matching is case-insensitive on the input (`board.yml`/directory names
/// are consistently lowercase in practice, but callers shouldn't have to
/// know that), exact against the table otherwise — no prefix/fuzzy
/// matching, unlike `Registry::search_chips`, since a wrong-but-plausible
/// match here would silently pick the wrong physical target.
pub fn resolve(soc: &str) -> Result<String, UnmappedSoc> {
    let needle = soc.to_lowercase();
    let chip = SOC_TO_CHIP
        .iter()
        .find(|(k, _)| *k == needle)
        .map(|(_, v)| *v)
        .ok_or_else(|| UnmappedSoc(soc.to_string()))?;

    Registry::from_builtin_families()
        .get_target_by_name(chip)
        .map_err(|_| UnmappedSoc(soc.to_string()))?;

    Ok(chip.to_string())
}

/// Every probe-rs target name, optionally narrowed by a **case-insensitive
/// substring** filter — decision 34's `chip-list [filter]`.
///
/// Pure enumeration: no probe is opened, no hardware is touched, nothing is
/// attached to. Same posture as `detect-dev-bench`, so an unprivileged human
/// can run it with no board plugged in and no service running.
///
/// **Substring, deliberately not `Registry::search_chips`.** That method
/// prefix-matches (with `x` as a wildcard), which is the wrong shape for the
/// job this exists to do: someone who knows their part is a "54L15" and is
/// hunting for the string to add to `SOC_TO_CHIP` gets nothing
/// from a prefix search, because the name they want is `nRF54L15_xxAA`. The
/// whole point of the fallback is that the user does *not* already know how
/// the name starts. Note the contrast with [`resolve`] directly above, which
/// is exact-match on purpose — there, a wrong-but-plausible match silently
/// picks the wrong physical target; here, the human reads the list and picks.
///
/// Results are deduplicated and sorted, so the output is stable enough to
/// diff across probe-rs upgrades.
pub fn chip_list(filter: Option<&str>) -> Vec<String> {
    let needle = filter.map(|f| f.to_lowercase());
    let registry = Registry::from_builtin_families();

    let mut names: Vec<String> = registry
        .families()
        .iter()
        .flat_map(|family| family.variants.iter())
        .map(|variant| variant.name.to_string())
        .filter(|name| match &needle {
            Some(n) => name.to_lowercase().contains(n.as_str()),
            None => true,
        })
        .collect();

    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_list_unfiltered_returns_the_whole_registry() {
        let all = chip_list(None);
        // No exact count asserted — it tracks probe-rs's target database and
        // would break on every upgrade for no benefit. The floor is the point.
        assert!(all.len() > 100, "expected a substantial target list, got {}", all.len());
    }

    #[test]
    fn chip_list_is_sorted_and_deduplicated() {
        let all = chip_list(None);
        let mut expected = all.clone();
        expected.sort();
        expected.dedup();
        assert_eq!(all, expected);
    }

    /// The case decision 34 exists for: a human knows the part is a 54L15 and
    /// does not know the name starts with `nRF`. A prefix search returns
    /// nothing here, which is why this is a substring match.
    #[test]
    fn chip_list_matches_a_substring_not_just_a_prefix() {
        let hits = chip_list(Some("54L15"));
        assert!(!hits.is_empty(), "expected nRF54L15 variants");
        assert!(
            hits.iter().all(|h| h.to_lowercase().contains("54l15")),
            "every hit must contain the needle: {hits:?}"
        );
        assert!(
            hits.iter().any(|h| !h.to_lowercase().starts_with("54l15")),
            "the point of the substring match is hits the needle does not prefix: {hits:?}"
        );
    }

    #[test]
    fn chip_list_filter_is_case_insensitive() {
        assert_eq!(chip_list(Some("nrf54l15")), chip_list(Some("NRF54L15")));
    }

    #[test]
    fn chip_list_returns_empty_for_a_needle_that_matches_nothing() {
        assert!(chip_list(Some("definitely-not-a-real-chip")).is_empty());
    }

    /// `chip_list` is where the value for a new `SOC_TO_CHIP` entry is read
    /// out of, so what it prints has to be a name `resolve`'s own validation step
    /// (`get_target_by_name`) will accept. Checked rather than assumed.
    #[test]
    fn chip_list_names_are_resolvable_targets() {
        let registry = Registry::from_builtin_families();
        for name in chip_list(Some("nRF54")) {
            assert!(
                registry.get_target_by_name(&name).is_ok(),
                "chip_list offered {name}, which probe-rs will not resolve"
            );
        }
    }

    #[test]
    fn resolves_known_soc() {
        assert_eq!(resolve("nrf54l15").unwrap(), "nRF54L15");
    }

    #[test]
    fn matching_is_case_insensitive_on_input() {
        assert_eq!(resolve("NRF54L15").unwrap(), "nRF54L15");
        assert_eq!(resolve("Nrf54L15").unwrap(), "nRF54L15");
    }

    #[test]
    fn unmapped_soc_errors_naming_the_soc() {
        let err = resolve("esp32c3").unwrap_err();
        assert!(err.0 == "esp32c3");
        assert!(err.to_string().contains("esp32c3"));
    }

    /// The lossy series-name row: Zephyr's `stm32g0b1xx` covers sixteen
    /// probe-rs variants, and this asserts the one the table deliberately
    /// picks, so a future edit that "fixes" the package letter has to argue
    /// with a test rather than pass silently.
    #[test]
    fn resolves_stm32g0b1_series_to_the_512k_v_package() {
        assert_eq!(resolve("stm32g0b1xx").unwrap(), "STM32G0B1VE");
        assert_eq!(resolve("STM32G0B1XX").unwrap(), "STM32G0B1VE");
    }

    /// probe-rs does not put an STM32G0 in the vendor-tool refusal list, so
    /// the chip this table now hands out has to be flashable by the default
    /// backend — otherwise the mapping resolves and then dead-ends at
    /// `/flash`. Checked here because `flash_backend` matches on the probe-rs
    /// name this module produces, not on the Zephyr SoC name.
    #[test]
    fn the_stm32g0_target_is_not_refused_by_the_flash_backend() {
        assert!(!crate::flash_backend::requires_vendor_tool(
            &resolve("stm32g0b1xx").unwrap()
        ));
    }

    #[test]
    fn resolves_esp32c5() {
        assert_eq!(resolve("esp32c5").unwrap(), "esp32c5");
        assert_eq!(resolve("ESP32C5").unwrap(), "esp32c5");
    }

    #[test]
    fn every_table_entry_resolves_against_the_real_registry() {
        // Guards against the table drifting from probe-rs's actual target
        // database as probe-rs versions change — exactly the gap `resolve`
        // itself exists to close for a caller, checked here at the table
        // level so a bad entry fails a test run, not a live call.
        let registry = Registry::from_builtin_families();
        for (soc, chip) in SOC_TO_CHIP {
            assert!(
                registry.get_target_by_name(chip).is_ok(),
                "SOC_TO_CHIP entry ({soc}, {chip}) does not resolve in probe-rs's builtin registry"
            );
        }
    }
}
