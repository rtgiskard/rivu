//! Linux system fallback metadata, ranked by Fontconfig rather than by coverage.
//!
//! The resolver owns its configuration for its entire lifetime. It never opens font
//! files: callers map the returned file/index pairs into their font database.

use fontconfig_sys::{self as fc, constants::*};
use std::{
    collections::VecDeque,
    ffi::{CStr, CString, OsStr},
    fmt,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::{self, NonNull},
    sync::Arc,
};

const MAX_CACHE_ENTRIES: usize = 64;
const MAX_CACHE_KEY_BYTES: usize = 64 * 1024;
// Bound retained face records and owned path capacity, not loaded font data.
const MAX_CACHE_CANDIDATE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FontconfigStyle {
    Normal,
    Italic,
    Oblique,
}

impl FontconfigStyle {
    fn slant(self) -> i32 {
        match self {
            Self::Normal => FC_SLANT_ROMAN,
            Self::Italic => FC_SLANT_ITALIC,
            Self::Oblique => FC_SLANT_OBLIQUE,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FontconfigCandidate {
    /// Original filesystem bytes are preserved, including non-UTF-8 paths.
    pub(super) file: PathBuf,
    /// The complete FC_INDEX, including any named-instance bits.
    pub(super) index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FontconfigError {
    InvalidInput(&'static str),
    Operation(&'static str),
    PatternResult(&'static str, fc::FcResult),
}

impl fmt::Display for FontconfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(field) => write!(f, "invalid Fontconfig {field}"),
            Self::Operation(operation) => write!(f, "Fontconfig {operation} failed"),
            Self::PatternResult(field, result) => {
                write!(f, "Fontconfig {field} returned result {result}")
            }
        }
    }
}

impl std::error::Error for FontconfigError {}

type Candidates = Arc<[FontconfigCandidate]>;
type Result<T, E = FontconfigError> = std::result::Result<T, E>;

pub(super) struct FontconfigResolver {
    config: Config,
    cache: CandidateCache,
}

impl FontconfigResolver {
    /// Loads Fontconfig metadata once; no global current configuration is changed.
    pub(super) fn new() -> Result<Self> {
        // SAFETY: the returned configuration is exclusively owned by Config.
        let config = unsafe { fc::FcInitLoadConfigAndFonts() };
        let config =
            NonNull::new(config).ok_or(FontconfigError::Operation("FcInitLoadConfigAndFonts"))?;
        Ok(Self {
            config: Config(config),
            cache: CandidateCache::default(),
        })
    }

    /// Returns concrete faces in Fontconfig order, each adding coverage of text
    /// still uncovered by earlier returned faces. It does not seek a smaller or
    /// higher-coverage set. An empty successful result is a cacheable no-coverage
    /// result; FFI/configuration failures are errors and are never cached.
    ///
    /// `family` is the original requested family, not an already resolved alias.
    /// `weight` uses CSS/OpenType units (normal = 400, bold = 700). Text is the
    /// actual fallback text, not a representative character for its script.
    pub(super) fn resolve(
        &mut self,
        family: &str,
        locale: &str,
        weight: u16,
        style: FontconfigStyle,
        text: &str,
    ) -> Result<Candidates> {
        let query = Query {
            family,
            locale,
            weight,
            style,
            text,
        };
        let config = &mut self.config;
        self.cache.resolve(query, || config.resolve(query))
    }
}

struct Config(NonNull<fc::FcConfig>);

// SAFETY: Config owns its reference, exposes no pointers, and all Fontconfig
// operations require exclusive access. Fontconfig has no thread affinity.
unsafe impl Send for Config {}
// SAFETY: shared references expose no FFI operations or mutable state. This
// permits storing the resolver in an outer RwLock without another inner lock.
unsafe impl Sync for Config {}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: this is our single owned configuration reference. FcFini must
        // not be called: other text systems may also be using Fontconfig.
        unsafe { fc::FcConfigDestroy(self.0.as_ptr()) }
    }
}

impl Config {
    fn resolve(&mut self, query: Query<'_>) -> Result<Candidates> {
        let mut needed = NeededCharacters::new(query.text);
        if needed.0.is_empty() {
            return Ok(Arc::from([]));
        }
        let family = CString::new(query.family)
            .map_err(|_| FontconfigError::InvalidInput("family containing NUL"))?;
        let locale = CString::new(query.locale)
            .map_err(|_| FontconfigError::InvalidInput("locale containing NUL"))?;

        // SAFETY: all allocated objects immediately acquire an owning guard.
        // Pattern additions copy/reference their arguments according to the
        // Fontconfig API, so the local strings and charset may be dropped later.
        unsafe {
            let pattern = Pattern(
                NonNull::new(fc::FcPatternCreate())
                    .ok_or(FontconfigError::Operation("FcPatternCreate"))?,
            );
            let charset = CharSet(
                NonNull::new(fc::FcCharSetCreate())
                    .ok_or(FontconfigError::Operation("FcCharSetCreate"))?,
            );
            for &character in &needed.0 {
                check_bool(
                    fc::FcCharSetAddChar(charset.0.as_ptr(), character),
                    "FcCharSetAddChar",
                )?;
            }
            check_bool(
                fc::FcPatternAddString(
                    pattern.0.as_ptr(),
                    FC_FAMILY.as_ptr(),
                    family.as_ptr().cast(),
                ),
                "FcPatternAddString(family)",
            )?;
            if !query.locale.is_empty() {
                check_bool(
                    fc::FcPatternAddString(
                        pattern.0.as_ptr(),
                        FC_LANG.as_ptr(),
                        locale.as_ptr().cast(),
                    ),
                    "FcPatternAddString(lang)",
                )?;
            }
            check_bool(
                fc::FcPatternAddDouble(
                    pattern.0.as_ptr(),
                    FC_WEIGHT.as_ptr(),
                    fontconfig_weight(query.weight),
                ),
                "FcPatternAddDouble(weight)",
            )?;
            check_bool(
                fc::FcPatternAddInteger(pattern.0.as_ptr(), FC_SLANT.as_ptr(), query.style.slant()),
                "FcPatternAddInteger(slant)",
            )?;
            check_bool(
                fc::FcPatternAddCharSet(
                    pattern.0.as_ptr(),
                    FC_CHARSET.as_ptr(),
                    charset.0.as_ptr(),
                ),
                "FcPatternAddCharSet",
            )?;
            check_bool(
                fc::FcConfigSubstitute(self.0.as_ptr(), pattern.0.as_ptr(), fc::FcMatchPattern),
                "FcConfigSubstitute",
            )?;
            fc::FcDefaultSubstitute(pattern.0.as_ptr());

            let mut result = fc::FcResultNoMatch;
            // Do not ask Fontconfig to trim against whole-font coverage: we
            // select contributions to this text only, in exactly this order.
            let fonts = SortedFonts(
                NonNull::new(fc::FcFontSort(
                    self.0.as_ptr(),
                    pattern.0.as_ptr(),
                    0,
                    ptr::null_mut(),
                    &mut result,
                ))
                .ok_or(FontconfigError::Operation("FcFontSort"))?,
            );
            let set = fonts.0.as_ref();
            // An empty allocated set with NoMatch is valid. A null return is
            // not: allocation failures can also leave result == NoMatch.
            if result != fc::FcResultMatch && !(result == fc::FcResultNoMatch && set.nfont == 0) {
                return Err(FontconfigError::PatternResult("FcFontSort", result));
            }
            if set.nfont < 0 || (set.nfont > 0 && set.fonts.is_null()) {
                return Err(FontconfigError::Operation("invalid FcFontSort set"));
            }
            let mut candidates = Vec::new();
            for index in 0..set.nfont as usize {
                if needed.0.is_empty() {
                    break;
                }
                let font = *set.fonts.add(index);
                if font.is_null() {
                    return Err(FontconfigError::Operation("null sorted font"));
                }
                let mut coverage = ptr::null_mut();
                check_result(
                    fc::FcPatternGetCharSet(font, FC_CHARSET.as_ptr(), 0, &mut coverage),
                    "charset",
                )?;
                if coverage.is_null() {
                    return Err(FontconfigError::Operation("null font charset"));
                }
                let candidate = needed.select(
                    |character| fc::FcCharSetHasChar(coverage, character) != 0,
                    || candidate_from_pattern(font),
                )?;
                if let Some(candidate) = candidate {
                    candidates.push(candidate);
                }
            }
            Ok(candidates.into())
        }
    }
}

struct Pattern(NonNull<fc::FcPattern>);
impl Drop for Pattern {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the pattern reference.
        unsafe { fc::FcPatternDestroy(self.0.as_ptr()) }
    }
}

struct CharSet(NonNull<fc::FcCharSet>);
impl Drop for CharSet {
    fn drop(&mut self) {
        // SAFETY: this guard owns the charset reference, independently of any
        // reference retained by a pattern.
        unsafe { fc::FcCharSetDestroy(self.0.as_ptr()) }
    }
}

struct SortedFonts(NonNull<fc::FcFontSet>);
impl Drop for SortedFonts {
    fn drop(&mut self) {
        // SAFETY: FcFontSort returns a font set owned by the caller. Destroying
        // the set releases its references, not other owners' shared patterns.
        unsafe { fc::FcFontSetDestroy(self.0.as_ptr()) }
    }
}

unsafe fn candidate_from_pattern(font: *mut fc::FcPattern) -> Result<FontconfigCandidate> {
    let mut file = ptr::null_mut();
    let mut index = 0;
    // SAFETY: the caller keeps the sorted font set and its pattern alive.
    unsafe {
        check_result(
            fc::FcPatternGetString(font, FC_FILE.as_ptr(), 0, &mut file),
            "file",
        )?;
        check_result(
            fc::FcPatternGetInteger(font, FC_INDEX.as_ptr(), 0, &mut index),
            "index",
        )?;
        if file.is_null() {
            return Err(FontconfigError::Operation("null font file"));
        }
        let file = CStr::from_ptr(file.cast()).to_bytes();
        if file.is_empty() || index < 0 {
            return Err(FontconfigError::Operation("invalid font file/index"));
        }
        Ok(FontconfigCandidate {
            file: PathBuf::from(OsStr::from_bytes(file)),
            index: index as u32,
        })
    }
}

fn check_bool(value: fc::FcBool, operation: &'static str) -> Result<()> {
    if value == 0 {
        Err(FontconfigError::Operation(operation))
    } else {
        Ok(())
    }
}

fn check_result(value: fc::FcResult, field: &'static str) -> Result<()> {
    if value == fc::FcResultMatch {
        Ok(())
    } else {
        Err(FontconfigError::PatternResult(field, value))
    }
}

/// Mirrors FcWeightFromOpenTypeDouble, which is not exposed by these bindings.
fn fontconfig_weight(weight: u16) -> f64 {
    const WEIGHTS: &[(u16, f64)] = &[
        (0, 0.0),
        (100, 0.0),
        (200, 40.0),
        (300, 50.0),
        (350, 55.0),
        (380, 75.0),
        (400, 80.0),
        (500, 100.0),
        (600, 180.0),
        (700, 200.0),
        (800, 205.0),
        (900, 210.0),
        (1000, 215.0),
    ];
    let weight = weight.min(1000);
    for pair in WEIGHTS.windows(2) {
        let (lower, lower_value) = pair[0];
        let (upper, upper_value) = pair[1];
        if weight <= upper {
            return lower_value
                + f64::from(weight - lower) * (upper_value - lower_value)
                    / f64::from(upper - lower);
        }
    }
    unreachable!("weight is clamped to the final mapping entry")
}

struct NeededCharacters(Vec<u32>);
impl NeededCharacters {
    fn new(text: &str) -> Self {
        let mut characters: Vec<_> = text.chars().map(u32::from).collect();
        characters.sort_unstable();
        characters.dedup();
        Self(characters)
    }

    fn select<T>(
        &mut self,
        mut has_character: impl FnMut(u32) -> bool,
        candidate: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        if !self.0.iter().copied().any(&mut has_character) {
            return Ok(None);
        }
        // A failure to extract a concrete face must not consume its coverage.
        let candidate = candidate()?;
        self.0.retain(|&character| !has_character(character));
        Ok(Some(candidate))
    }
}

#[derive(Clone, Copy)]
struct Query<'a> {
    family: &'a str,
    locale: &'a str,
    weight: u16,
    style: FontconfigStyle,
    text: &'a str,
}

impl Query<'_> {
    fn key_bytes(self) -> usize {
        self.family
            .len()
            .saturating_add(self.locale.len())
            .saturating_add(self.text.len())
    }
}

struct CacheEntry {
    family: Box<str>,
    locale: Box<str>,
    weight: u16,
    style: FontconfigStyle,
    text: Box<str>,
    candidates: Candidates,
    candidate_bytes: usize,
}

impl CacheEntry {
    fn matches(&self, query: Query<'_>) -> bool {
        self.weight == query.weight
            && self.style == query.style
            && &*self.family == query.family
            && &*self.locale == query.locale
            && &*self.text == query.text
    }

    fn key_bytes(&self) -> usize {
        self.family.len() + self.locale.len() + self.text.len()
    }
}

/// Small FIFO memoization. Hits do not allocate query strings or refresh age.
/// Oversized keys or candidate storage still resolve normally, without retention
/// or truncation. Empty and nonempty successes share the same budgets.
#[derive(Default)]
struct CandidateCache {
    entries: VecDeque<CacheEntry>,
    key_bytes: usize,
    candidate_bytes: usize,
}

impl CandidateCache {
    fn resolve(
        &mut self,
        query: Query<'_>,
        load: impl FnOnce() -> Result<Candidates>,
    ) -> Result<Candidates> {
        if let Some(entry) = self.entries.iter().find(|entry| entry.matches(query)) {
            return Ok(entry.candidates.clone());
        }
        let candidates = load()?;
        let key_bytes = query.key_bytes();
        if key_bytes > MAX_CACHE_KEY_BYTES {
            return Ok(candidates);
        }
        if let Some(candidate_bytes) = retained_candidate_bytes(&candidates)
            .filter(|&bytes| bytes <= MAX_CACHE_CANDIDATE_BYTES)
        {
            while self.entries.len() >= MAX_CACHE_ENTRIES
                || key_bytes > MAX_CACHE_KEY_BYTES - self.key_bytes
                || candidate_bytes > MAX_CACHE_CANDIDATE_BYTES - self.candidate_bytes
            {
                if let Some(entry) = self.entries.pop_front() {
                    self.key_bytes -= entry.key_bytes();
                    self.candidate_bytes -= entry.candidate_bytes;
                }
            }
            self.key_bytes += key_bytes;
            self.candidate_bytes += candidate_bytes;
            self.entries.push_back(CacheEntry {
                family: query.family.into(),
                locale: query.locale.into(),
                weight: query.weight,
                style: query.style,
                text: query.text.into(),
                candidates: candidates.clone(),
                candidate_bytes,
            });
        }
        Ok(candidates)
    }
}

fn retained_candidate_bytes(candidates: &[FontconfigCandidate]) -> Option<usize> {
    let items = candidates
        .len()
        .checked_mul(std::mem::size_of::<FontconfigCandidate>())?;
    candidates.iter().try_fold(items, |bytes, candidate| {
        bytes.checked_add(candidate.file.capacity())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> Query<'_> {
        Query {
            family: "original",
            locale: "en-US",
            weight: 400,
            style: FontconfigStyle::Normal,
            text,
        }
    }

    fn empty() -> Result<Candidates> {
        Ok(Arc::from([]))
    }

    fn select(text: &str, fonts: &[&str]) -> Vec<usize> {
        let mut needed = NeededCharacters::new(text);
        let mut selected = Vec::new();
        for (index, coverage) in fonts.iter().enumerate() {
            if needed.0.is_empty() {
                break;
            }
            if let Some(index) = needed
                .select(
                    |character| coverage.chars().any(|c| u32::from(c) == character),
                    || Ok(index),
                )
                .unwrap()
            {
                selected.push(index);
            }
        }
        selected
    }

    #[test]
    fn preserves_rank_instead_of_maximizing_coverage() {
        assert_eq!(select("ab", &["a", "ab"]), vec![0, 1]);
        assert_eq!(select("ab", &["ab", "a", "b"]), vec![0]);
        assert_eq!(select("ab", &["x", "a", "a", "b"]), vec![1, 3]);
    }

    #[test]
    fn uses_actual_unique_text_characters() {
        assert_eq!(select("a日a本", &["日", "中", "a", "本"]), vec![0, 2, 3]);
        assert!(select("日", &["a", "中"]).is_empty());
        assert!(select("", &["a"]).is_empty());
    }

    #[test]
    fn does_not_cap_number_of_contributing_faces() {
        let fonts: Vec<_> = (0..100)
            .map(|i| char::from_u32(0x4e00 + i).unwrap().to_string())
            .collect();
        let text = fonts.concat();
        let coverages: Vec<_> = fonts.iter().map(String::as_str).collect();
        assert_eq!(select(&text, &coverages), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn failed_candidate_does_not_consume_coverage() {
        let mut needed = NeededCharacters::new("a");
        let result = needed.select(
            |_| true,
            || Err::<(), _>(FontconfigError::Operation("test")),
        );
        assert!(result.is_err());
        assert_eq!(needed.0, vec![u32::from('a')]);
    }

    #[test]
    fn caches_no_coverage_but_not_ffi_failures() {
        let mut cache = CandidateCache::default();
        assert!(
            cache
                .resolve(query("a"), || Err(FontconfigError::Operation("test")))
                .is_err()
        );
        assert!(cache.entries.is_empty());
        cache.resolve(query("a"), empty).unwrap();
        cache
            .resolve(query("a"), || panic!("negative result should be memoized"))
            .unwrap();
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn cache_key_keeps_every_request_dimension() {
        let mut cache = CandidateCache::default();
        let base = query("a");
        for request in [
            base,
            Query {
                family: "alias",
                ..base
            },
            Query {
                locale: "ja",
                ..base
            },
            Query {
                weight: 700,
                ..base
            },
            Query {
                style: FontconfigStyle::Italic,
                ..base
            },
            Query {
                style: FontconfigStyle::Oblique,
                ..base
            },
            Query { text: "b", ..base },
        ] {
            cache.resolve(request, empty).unwrap();
        }
        assert_eq!(cache.entries.len(), 7);
    }

    #[test]
    fn cache_bounds_entry_count_and_aggregate_key_bytes() {
        let mut cache = CandidateCache::default();
        for index in 0..MAX_CACHE_ENTRIES + 1 {
            cache.resolve(query(&index.to_string()), empty).unwrap();
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!cache.entries.iter().any(|entry| &*entry.text == "0"));

        let text = "a".repeat(MAX_CACHE_KEY_BYTES / 2);
        cache.resolve(query(&text), empty).unwrap();
        let other_text = "b".repeat(MAX_CACHE_KEY_BYTES / 2);
        cache.resolve(query(&other_text), empty).unwrap();
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.key_bytes <= MAX_CACHE_KEY_BYTES);
        assert_eq!(
            cache.key_bytes,
            cache.entries.iter().map(CacheEntry::key_bytes).sum::<usize>()
        );
    }

    #[test]
    fn oversized_requests_resolve_without_being_memoized() {
        let mut cache = CandidateCache::default();
        let text = "a".repeat(MAX_CACHE_KEY_BYTES + 1);
        for _ in 0..2 {
            cache.resolve(query(&text), empty).unwrap();
            assert!(cache.entries.is_empty());
            assert_eq!(cache.key_bytes, 0);
        }
    }

    #[test]
    fn cache_hits_do_not_refresh_fifo_age() {
        let mut cache = CandidateCache::default();
        for index in 0..MAX_CACHE_ENTRIES {
            cache.resolve(query(&index.to_string()), empty).unwrap();
        }
        cache
            .resolve(query("0"), || panic!("expected a cache hit"))
            .unwrap();
        cache.resolve(query("new"), empty).unwrap();
        assert_eq!(&*cache.entries.front().unwrap().text, "1");
        assert!(!cache.entries.iter().any(|entry| &*entry.text == "0"));
    }

    #[test]
    fn candidate_item_storage_has_an_inclusive_budget() {
        let mut cache = CandidateCache::default();
        let item_bytes = std::mem::size_of::<FontconfigCandidate>();
        let count = MAX_CACHE_CANDIDATE_BYTES / item_bytes;
        let candidates: Candidates = vec![
            FontconfigCandidate {
                file: PathBuf::new(),
                index: 0,
            };
            count
        ]
        .into();
        let returned = cache
            .resolve(query("at-budget"), || Ok(candidates.clone()))
            .unwrap();
        assert!(Arc::ptr_eq(&returned, &candidates));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.candidate_bytes, count * item_bytes);
        cache
            .resolve(query("at-budget"), || panic!("expected a cache hit"))
            .unwrap();

        let oversized: Candidates = vec![
            FontconfigCandidate {
                file: PathBuf::new(),
                index: u32::MAX,
            };
            count + 1
        ]
        .into();
        for _ in 0..2 {
            let returned = cache
                .resolve(query("over-budget"), || Ok(oversized.clone()))
                .unwrap();
            assert!(Arc::ptr_eq(&returned, &oversized));
            assert_eq!(returned.len(), count + 1);
            assert_eq!(returned.last().unwrap().index, u32::MAX);
            assert_eq!(cache.entries.len(), 1);
            assert_eq!(cache.candidate_bytes, count * item_bytes);
        }
    }

    #[test]
    fn candidate_budget_counts_unused_path_capacity() {
        let mut cache = CandidateCache::default();
        cache.resolve(query("retained"), empty).unwrap();
        let mut file = PathBuf::with_capacity(MAX_CACHE_CANDIDATE_BYTES);
        file.push("a");
        let candidates: Candidates = vec![FontconfigCandidate { file, index: 7 }].into();
        assert!(candidates[0].file.as_os_str().len() < MAX_CACHE_CANDIDATE_BYTES);
        assert!(retained_candidate_bytes(&candidates).unwrap() > MAX_CACHE_CANDIDATE_BYTES);
        for _ in 0..2 {
            let returned = cache
                .resolve(query("large-path"), || Ok(candidates.clone()))
                .unwrap();
            assert!(Arc::ptr_eq(&returned, &candidates));
            assert_eq!(cache.entries.len(), 1);
            assert_eq!(cache.candidate_bytes, 0);
        }
    }

    #[test]
    fn aggregate_candidate_storage_evicts_oldest_and_releases_accounting() {
        let mut cache = CandidateCache::default();
        let candidates: Candidates = vec![FontconfigCandidate {
            file: PathBuf::from("a".repeat(MAX_CACHE_CANDIDATE_BYTES / 2)),
            index: 9,
        }]
        .into();
        let bytes = retained_candidate_bytes(&candidates).unwrap();
        assert!(bytes <= MAX_CACHE_CANDIDATE_BYTES);
        assert!(bytes > MAX_CACHE_CANDIDATE_BYTES / 2);
        cache
            .resolve(query("first"), || Ok(candidates.clone()))
            .unwrap();
        cache
            .resolve(query("second"), || Ok(candidates.clone()))
            .unwrap();
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(&*cache.entries.front().unwrap().text, "second");
        assert_eq!(cache.candidate_bytes, bytes);
        assert_eq!(cache.key_bytes, query("second").key_bytes());

        // Key-budget eviction must release candidate accounting too.
        let text = "b".repeat(MAX_CACHE_KEY_BYTES - query("").key_bytes());
        cache.resolve(query(&text), empty).unwrap();
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.key_bytes, MAX_CACHE_KEY_BYTES);
        assert_eq!(cache.candidate_bytes, 0);
    }

    #[test]
    fn converts_numeric_weight_and_style_to_fontconfig_units() {
        assert_eq!(fontconfig_weight(100), 0.0);
        assert_eq!(fontconfig_weight(350), 55.0);
        assert_eq!(fontconfig_weight(400), 80.0);
        assert_eq!(fontconfig_weight(450), 90.0);
        assert_eq!(fontconfig_weight(700), 200.0);
        assert_eq!(fontconfig_weight(1000), 215.0);
        assert_eq!(fontconfig_weight(u16::MAX), 215.0);
        assert_eq!(FontconfigStyle::Normal.slant(), FC_SLANT_ROMAN);
        assert_eq!(FontconfigStyle::Italic.slant(), FC_SLANT_ITALIC);
        assert_eq!(FontconfigStyle::Oblique.slant(), FC_SLANT_OBLIQUE);
    }
}
