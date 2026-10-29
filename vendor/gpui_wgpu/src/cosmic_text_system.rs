#[cfg(target_os = "linux")]
use crate::fontconfig::{FontconfigCandidate, FontconfigResolver, FontconfigStyle};
use anyhow::{Context as _, Ok, Result};
#[cfg(target_os = "linux")]
use collections::HashSet;
use collections::HashMap;
use cosmic_text::{
    Attrs, AttrsList, Ellipsize, Family, Font as CosmicTextFont,
    FontFeatures as CosmicFontFeatures, FontSystem, ShapeBuffer, ShapeLine, Stretch, Style, Weight,
};
#[cfg(test)]
use gpui::FontFallbacks;
use gpui::{
    Bounds, DevicePixels, FallbackFontClass, Font, FontFeatures, FontId, FontMetrics, FontRun,
    GlyphId, IsZero as _, LineLayout, MissingGlyph, MissingGlyphSink, Pixels, PlatformTextSystem,
    RenderGlyphParams, SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ShapedGlyph, ShapedRun,
    SharedString, Size, TextRenderingMode, point, size,
};

use itertools::Itertools;
use parking_lot::RwLock;
use smallvec::SmallVec;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::{borrow::Cow, ops::Range, sync::Arc};
use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith},
    zeno::{Format, Vector},
};
use unicode_segmentation::UnicodeSegmentation;

// One immediate bounds-to-rasterize handoff, not a glyph residency cache.
// Larger bitmaps remain renderable without retaining more than 256 KiB here.
const MAX_PENDING_GLYPH_BYTES: usize = 256 * 1024;

pub struct CosmicTextSystem(RwLock<CosmicTextSystemState>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LoadedFontKey {
    database_id: cosmic_text::fontdb::ID,
    features: FontFeatures,
    fallback_chain: SmallVec<[(FontId, SharedString); 4]>,
    /// System resolution needs the original requested family, weight, and style.
    system_request: Option<Font>,
}

struct CosmicTextSystemState {
    font_system: FontSystem,
    scratch: ShapeBuffer,
    swash_scale_context: ScaleContext,
    pending_glyph_image: Option<(RenderGlyphParams, swash::scale::image::Image)>,
    /// Contains selected faces and native shaping fallbacks. Indexed by `FontId`.
    loaded_fonts: Vec<LoadedFont>,
    loaded_font_ids_by_key: HashMap<LoadedFontKey, FontId>,
    /// Primary requests require usable Latin metrics; fallback-only requests do not.
    font_ids_by_request: HashMap<(Font, bool), FontId>,
    #[cfg(target_os = "linux")]
    fontconfig: Option<Result<FontconfigResolver, String>>,
    #[cfg(target_os = "linux")]
    fontconfig_face_ids: HashMap<PathBuf, HashMap<u32, Option<cosmic_text::fontdb::ID>>>,
    system_font_fallback: String,
    missing_glyph_sink: Option<Arc<dyn MissingGlyphSink>>,
}

struct LoadedFont {
    font: Arc<CosmicTextFont>,
    request: Font,
    features: CosmicFontFeatures,
    is_known_emoji_font: bool,
    /// resolved at load time so `layout_line` shares one chain across faces.
    /// `Arc` keeps clone cheap on the per-run hot path.
    user_fallback_chain: Arc<[(FontId, SharedString)]>,
}

struct FontMatchProperties {
    primary_family_name: SharedString,
    stretch: Stretch,
    style: Style,
    weight: Weight,
    features: CosmicFontFeatures,
    fallback_chain: Arc<[(FontId, SharedString)]>,
}

impl FontMatchProperties {
    fn attributes<'a>(&'a self, font_id: FontId, family_name: &'a str) -> Attrs<'a> {
        Attrs::new()
            .metadata(font_id.0)
            .family(Family::Name(family_name))
            .stretch(self.stretch)
            .style(self.style)
            .weight(self.weight)
            .font_features(self.features.clone())
    }
}

impl CosmicTextSystem {
    /// Returns the selected face's weight and style, which may differ from the request.
    pub fn font_weight_and_style(
        &self,
        font_id: FontId,
    ) -> Result<(gpui::FontWeight, gpui::FontStyle)> {
        let state = self.0.read();
        let font = state
            .loaded_fonts
            .get(font_id.0)
            .context("invalid font ID")?;
        let face = state
            .font_system
            .db()
            .face(font.font.id())
            .context("font face not found")?;
        let style = match face.style {
            cosmic_text::Style::Normal => gpui::FontStyle::Normal,
            cosmic_text::Style::Italic => gpui::FontStyle::Italic,
            cosmic_text::Style::Oblique => gpui::FontStyle::Oblique,
        };
        Ok((gpui::FontWeight(face.weight.0 as f32), style))
    }

    /// Builds reports for unresolved source indices after an outer text system
    /// has applied its own fallback.
    pub fn missing_glyphs(
        &self,
        text: &str,
        font_runs: &[FontRun],
        missing_text_indices: impl IntoIterator<Item = usize>,
    ) -> Vec<MissingGlyph> {
        self.0
            .read()
            .missing_glyphs(text, font_runs, missing_text_indices)
    }

    pub fn new(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new();

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            scratch: ShapeBuffer::default(),
            swash_scale_context: ScaleContext::new(),
            pending_glyph_image: None,
            loaded_fonts: Vec::new(),
            loaded_font_ids_by_key: HashMap::default(),
            font_ids_by_request: HashMap::default(),
            #[cfg(target_os = "linux")]
            fontconfig: None,
            #[cfg(target_os = "linux")]
            fontconfig_face_ids: HashMap::default(),
            system_font_fallback: system_font_fallback.to_string(),
            missing_glyph_sink: None,
        }))
    }

    pub fn new_without_system_fonts(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new_with_locale_and_db(
            "en-US".to_string(),
            cosmic_text::fontdb::Database::new(),
        );

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            scratch: ShapeBuffer::default(),
            swash_scale_context: ScaleContext::new(),
            pending_glyph_image: None,
            loaded_fonts: Vec::new(),
            loaded_font_ids_by_key: HashMap::default(),
            font_ids_by_request: HashMap::default(),
            #[cfg(target_os = "linux")]
            fontconfig: None,
            #[cfg(target_os = "linux")]
            fontconfig_face_ids: HashMap::default(),
            system_font_fallback: system_font_fallback.to_string(),
            missing_glyph_sink: None,
        }))
    }
}

impl PlatformTextSystem for CosmicTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn set_missing_glyph_sink(&self, sink: Option<Arc<dyn MissingGlyphSink>>) {
        self.0.write().missing_glyph_sink = sink;
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut result = self
            .0
            .read()
            .font_system
            .db()
            .faces()
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect_vec();
        result.sort_unstable();
        result.dedup();
        result
    }

    #[cfg(target_os = "linux")]
    fn resolve_system_font_family(&self, requested: &str) -> Result<String> {
        let available = self.all_font_names();
        let mut state = self.0.write();
        let resolver = CosmicTextSystemState::fontconfig_resolver(&mut state.fontconfig)?;
        Ok(resolver.primary_family(requested, &available)?)
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let mut state = self.0.write();
        state
            .load_family(font, true)?
            .context("requested font family contains no font matching the other parameters")
    }

    fn prewarm_fonts(&self, font_ids: &[FontId]) {
        self.0.write().prewarm_fonts(font_ids);
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self
            .0
            .read()
            .loaded_font(font_id)
            .font
            .as_swash()
            .metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent,
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.0.read();
        let glyph_metrics = lock.loaded_font(font_id).font.as_swash().glyph_metrics(&[]);
        let glyph_id = glyph_id.0 as u16;
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Subpixel
    }
}

impl CosmicTextSystemState {
    #[cfg(target_os = "linux")]
    fn fontconfig_resolver(
        slot: &mut Option<Result<FontconfigResolver, String>>,
    ) -> Result<&mut FontconfigResolver> {
        slot.get_or_insert_with(|| FontconfigResolver::new().map_err(|error| error.to_string()))
            .as_mut()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    fn loaded_font(&self, font_id: FontId) -> &LoadedFont {
        &self.loaded_fonts[font_id.0]
    }

    fn font_match_properties(&self, font_id: FontId) -> Option<FontMatchProperties> {
        let loaded_font = self.loaded_font(font_id);
        let Some(face) = self.font_system.db().face(loaded_font.font.id()) else {
            log::warn!("font face not found in database for font_id {:?}", font_id);
            return None;
        };
        let Some(first_family) = face.families.first() else {
            log::warn!("font face has no family names for font_id {:?}", font_id);
            return None;
        };

        Some(FontMatchProperties {
            primary_family_name: first_family.0.clone().into(),
            stretch: face.stretch,
            style: face.style,
            weight: face.weight,
            features: loaded_font.features.clone(),
            fallback_chain: Arc::clone(&loaded_font.user_fallback_chain),
        })
    }
    fn face_attributes<'a>(
        &self,
        font_id: FontId,
        family_name: &'a str,
        properties: &'a FontMatchProperties,
    ) -> Attrs<'a> {
        let attributes = properties.attributes(font_id, family_name);
        match self
            .font_system
            .db()
            .face(self.loaded_font(font_id).font.id())
        {
            Some(face) => attributes
                .stretch(face.stretch)
                .style(face.style)
                .weight(face.weight),
            None => attributes,
        }
    }

    /// Resolves text-specific fallback before shaping. The registered font's
    /// explicit chain remains immutable and platform fonts stay lazy.
    fn prepare_font(&mut self, font_id: FontId, text: &str) -> Option<FontMatchProperties> {
        let properties = self.font_match_properties(font_id)?;
        #[cfg(target_os = "linux")]
        {
            let mut properties = properties;
            if let Err(error) = self.prepare_system_fallbacks(font_id, text, &mut properties) {
                log::warn!("could not prepare system font fallback: {error:#}");
            }
            Some(properties)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = text;
            Some(properties)
        }
    }

    #[cfg(target_os = "linux")]
    fn prepare_system_fallbacks(
        &mut self,
        primary: FontId,
        text: &str,
        properties: &mut FontMatchProperties,
    ) -> Result<()> {
        let request = &self.loaded_font(primary).request;
        let Some(_) = request
            .fallbacks
            .as_ref()
            .and_then(|fallbacks| fallbacks.system_family(&request.family))
        else {
            return Ok(());
        };
        let uncovered = |character: char| {
            needs_glyph_coverage(character)
                && !charmap_covers(&self.loaded_fonts, primary, character)
                && !properties
                    .fallback_chain
                    .iter()
                    .any(|(id, _)| charmap_covers(&self.loaded_fonts, *id, character))
        };
        let (missing_text, mut remaining) = missing_fallback_coverage(text, uncovered);
        if missing_text.is_empty() {
            return Ok(());
        }

        let request = request.clone();
        let family = request
            .fallbacks
            .as_ref()
            .and_then(|fallbacks| fallbacks.system_family(&request.family))
            .context("system fallback request disappeared")?;
        let style = match request.style {
            gpui::FontStyle::Normal => FontconfigStyle::Normal,
            gpui::FontStyle::Italic => FontconfigStyle::Italic,
            gpui::FontStyle::Oblique => FontconfigStyle::Oblique,
        };
        let resolver = Self::fontconfig_resolver(&mut self.fontconfig)?;
        let candidates = resolver.resolve(
            family,
            self.font_system.locale(),
            request.weight.0 as u16,
            style,
            &missing_text,
        )?;
        self.append_system_fallbacks(primary, &request, &candidates, &mut remaining, properties);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn append_system_fallbacks(
        &mut self,
        primary: FontId,
        request: &Font,
        candidates: &[FontconfigCandidate],
        remaining: &mut Vec<char>,
        properties: &mut FontMatchProperties,
    ) {
        let mut chain = properties.fallback_chain.to_vec();
        let mut selected_faces: HashSet<_> = chain
            .iter()
            .map(|(id, _)| self.loaded_font(*id).font.id())
            .collect();
        selected_faces.insert(self.loaded_font(primary).font.id());
        for candidate in candidates {
            let database_id = match self.fontconfig_face_id(candidate) {
                Result::Ok(Some(id)) => id,
                Result::Ok(None) => continue,
                Err(error) => {
                    log::warn!("could not register system fallback {candidate:?}: {error:#}");
                    continue;
                }
            };
            if selected_faces.contains(&database_id) {
                continue;
            }
            let Some(face) = self.font_system.db().face(database_id) else {
                continue;
            };
            let Some((family, _)) = face.families.first() else {
                continue;
            };
            let family: SharedString = family.clone().into();
            let fallback_request = Font {
                family: family.clone(),
                fallbacks: None,
                ..request.clone()
            };
            match self.load_face(database_id, &fallback_request, false) {
                Result::Ok(Some(font_id)) => {
                    selected_faces.insert(database_id);
                    chain.push((font_id, family));
                    remaining.retain(|&character| {
                        !charmap_covers(&self.loaded_fonts, font_id, character)
                    });
                    if remaining.is_empty() {
                        break;
                    }
                }
                Result::Ok(None) => {}
                Err(error) => {
                    log::warn!("could not load system fallback {candidate:?}: {error:#}");
                }
            }
        }
        properties.fallback_chain = Arc::from(chain);
    }

    #[cfg(target_os = "linux")]
    fn fontconfig_face_id(
        &mut self,
        candidate: &FontconfigCandidate,
    ) -> Result<Option<cosmic_text::fontdb::ID>> {
        if let Some(&id) = self
            .fontconfig_face_ids
            .get(&candidate.file)
            .and_then(|faces| faces.get(&candidate.index))
        {
            return Ok(id);
        }
        // FC_INDEX packs a named-instance index in the high bits. fontdb has
        // collection faces only, so map the base face, never the packed value.
        // Named-instance coordinates beyond cosmic's weight/style matching are
        // not representable in fontdb; this intentionally uses the base face.
        let index = candidate.index & 0xffff;
        let matching_face = |face: &cosmic_text::fontdb::FaceInfo| {
            face.index == index
                && match &face.source {
                    cosmic_text::fontdb::Source::File(path)
                    | cosmic_text::fontdb::Source::SharedFile(path, _) => path == &candidate.file,
                    cosmic_text::fontdb::Source::Binary(_) => false,
                }
        };
        let mut id = self
            .font_system
            .db()
            .faces()
            .find(|face| matching_face(face))
            .map(|face| face.id);
        if id.is_none() {
            self.font_system.db_mut().load_font_file(&candidate.file)?;
            self.font_ids_by_request.clear();
            id = self
                .font_system
                .db()
                .faces()
                .find(|face| matching_face(face))
                .map(|face| face.id);
        }
        self.fontconfig_face_ids
            .entry(candidate.file.clone())
            .or_default()
            .insert(candidate.index, id);
        Ok(id)
    }

    fn prewarm_fonts(&mut self, font_ids: &[FontId]) {
        for &font_id in font_ids {
            let Some(properties) = self.font_match_properties(font_id) else {
                continue;
            };
            let primary_attributes =
                properties.attributes(font_id, &properties.primary_family_name);
            self.font_system.get_font_matches(&primary_attributes);

            for (fallback_id, fallback_name) in &*properties.fallback_chain {
                let fallback_attributes =
                    self.face_attributes(*fallback_id, fallback_name, &properties);
                self.font_system.get_font_matches(&fallback_attributes);
            }
        }
    }

    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.font_ids_by_request.clear();
        let db = self.font_system.db_mut();
        for bytes in fonts {
            db.load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(bytes)));
        }
        Ok(())
    }

    #[profiling::function]
    fn load_family(&mut self, request: &Font, primary: bool) -> Result<Option<FontId>> {
        let request_key = (request.clone(), primary);
        if let Some(&font_id) = self.font_ids_by_request.get(&request_key) {
            return Ok(Some(font_id));
        }

        let name = gpui::font_name_with_fallbacks(&request.family, &self.system_font_fallback);
        let mut candidates = self
            .font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| name == family.0))
            .map(|face| face.id)
            .collect::<SmallVec<[_; 4]>>();
        while !candidates.is_empty() {
            let ix = find_best_match(request, &candidates, self.font_system.db())?;
            if let Some(font_id) = self.load_face(candidates[ix], request, primary)? {
                self.font_ids_by_request.insert(request_key, font_id);
                return Ok(Some(font_id));
            }
            candidates.remove(ix);
        }

        Ok(None)
    }

    /// Loads one already-selected database face. Fallback-only faces need not
    /// provide the Latin metrics required of a primary font.
    fn load_face(
        &mut self,
        database_id: cosmic_text::fontdb::ID,
        request: &Font,
        primary: bool,
    ) -> Result<Option<FontId>> {
        let font = self
            .font_system
            .get_font(database_id, cosmic_text::Weight::NORMAL)
            .context("Could not load font")?;
        let face = self
            .font_system
            .db()
            .face(database_id)
            .context("font face not found in database")?;

        // Primary fonts supply the editor's 'm' advance. Fallback-only
        // faces may be Arabic, Devanagari, or emoji without any Latin.
        // Keep the caption-icon exception used by the storybook.
        let allowed_bad_font_names = ["SegoeFluentIcons", "Segoe Fluent Icons"];
        if primary
            && font.as_swash().charmap().map('m') == 0
            && !allowed_bad_font_names.contains(&face.post_script_name.as_str())
        {
            return Ok(None);
        }
        let is_known_emoji_font = check_is_known_emoji_font(&face.post_script_name);

        // Resolve each explicit family with the same weight and style as
        // the request, without letting it recursively inherit the chain.
        // A system-family descriptor has an empty explicit list and stays lazy.
        let mut chain = SmallVec::new();
        if let Some(fallbacks) = &request.fallbacks {
            for fallback_name in fallbacks.fallback_list() {
                let fallback = Font {
                    family: fallback_name.clone().into(),
                    fallbacks: None,
                    ..request.clone()
                };
                let Some(fallback_id) = self.load_family(&fallback, false)? else {
                    continue;
                };
                let fallback_database_id = self.loaded_font(fallback_id).font.id();
                if let Some(face) = self.font_system.db().face(fallback_database_id)
                    && let Some(family) = face.families.first()
                {
                    chain.push((fallback_id, SharedString::from(family.0.clone())));
                }
            }
        }
        let key = LoadedFontKey {
            database_id,
            features: request.features.clone(),
            fallback_chain: chain,
            system_request: request
                .fallbacks
                .as_ref()
                .and_then(|fallbacks| fallbacks.system_family(&request.family))
                .map(|_| request.clone()),
        };
        let font_id = if let Some(&font_id) = self.loaded_font_ids_by_key.get(&key) {
            font_id
        } else {
            let cosmic_features = cosmic_font_features(&request.features)?;
            let fallback_chain = Arc::from(key.fallback_chain.as_slice());
            let font_id = FontId(self.loaded_fonts.len());
            self.loaded_fonts.push(LoadedFont {
                font,
                request: request.clone(),
                features: cosmic_features,
                is_known_emoji_font,
                user_fallback_chain: fallback_chain,
            });
            self.loaded_font_ids_by_key.insert(key, font_id);
            font_id
        };
        Ok(Some(font_id))
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let glyph_metrics = self.loaded_font(font_id).font.as_swash().glyph_metrics(&[]);
        Ok(Size {
            width: glyph_metrics.advance_width(glyph_id.0 as u16),
            height: glyph_metrics.advance_height(glyph_id.0 as u16),
        })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.loaded_font(font_id).font.as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        // Atlas hits may never consume this handoff. Release it before rendering
        // another request, including empty glyphs and failed renders.
        self.pending_glyph_image = None;
        let image = self.render_glyph_image(params)?;
        let bounds = Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        };
        if !bounds.is_zero() && image.data.capacity() <= MAX_PENDING_GLYPH_BYTES {
            self.pending_glyph_image = Some((params.clone(), image));
        }
        Ok(bounds)
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        let mut image = match self
            .pending_glyph_image
            .take_if(|(cached, _)| cached == params)
        {
            Some((_, image)) => image,
            None => self.render_glyph_image(params)?,
        };
        let bitmap_size = glyph_bounds.size;
        match image.content {
            swash::scale::image::Content::Color | swash::scale::image::Content::SubpixelMask => {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Ok((bitmap_size, image.data))
            }
            swash::scale::image::Content::Mask => {
                if params.subpixel_rendering {
                    // We must always return RGBA data when subpixel rendering is requested.
                    let expanded = image.data.iter().flat_map(|&a| [a, a, a, a]).collect();
                    Ok((bitmap_size, expanded))
                } else {
                    Ok((bitmap_size, image.data))
                }
            }
        }
    }

    fn render_glyph_image(
        &mut self,
        params: &RenderGlyphParams,
    ) -> Result<swash::scale::image::Image> {
        let loaded_font = &self.loaded_fonts[params.font_id.0];
        let font_ref = loaded_font.font.as_swash();
        let pixel_size = f32::from(params.font_size);

        let subpixel_offset = Vector::new(
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
            params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
        );

        let mut scaler = self
            .swash_scale_context
            .builder(font_ref)
            .size(pixel_size * params.scale_factor)
            .hint(true)
            .build();

        let sources: &[Source] = if params.is_emoji {
            &[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ]
        } else {
            &[Source::Bitmap(StrikeWith::ExactSize), Source::Outline]
        };

        let mut renderer = Render::new(sources);
        if params.subpixel_rendering {
            // There seems to be a bug in Swash where the B and R values are swapped.
            renderer
                .format(Format::subpixel_bgra())
                .offset(subpixel_offset);
        } else {
            renderer.format(Format::Alpha).offset(subpixel_offset);
        }

        let glyph_id: u16 = params.glyph_id.0.try_into()?;
        renderer
            .render(&mut scaler, glyph_id)
            .with_context(|| format!("unable to render glyph via swash for {params:?}"))
    }

    /// This is used when cosmic_text has chosen a fallback font instead of using the requested
    /// font, typically to handle some unicode characters. When this happens, `loaded_fonts` may not
    /// yet have an entry for this fallback font, and so one is added.
    ///
    /// Note that callers shouldn't use this `FontId` somewhere that will retrieve the corresponding
    /// `LoadedFont.features`, as it will have an arbitrarily chosen or empty value. The only
    /// current use of this field is for the *input* of `layout_line`, and so it's fine to use
    /// `font_id_for_cosmic_id` when computing the *output* of `layout_line`.
    fn font_id_for_cosmic_id(&mut self, id: cosmic_text::fontdb::ID) -> Result<FontId> {
        if let Some(ix) = self
            .loaded_fonts
            .iter()
            .position(|loaded_font| loaded_font.font.id() == id)
        {
            Ok(FontId(ix))
        } else {
            let font = self
                .font_system
                .get_font(id, cosmic_text::Weight::NORMAL)
                .context("failed to get fallback font from cosmic-text font system")?;
            let face = self
                .font_system
                .db()
                .face(id)
                .context("fallback font face not found in cosmic-text database")?;

            let font_id = FontId(self.loaded_fonts.len());
            self.loaded_fonts.push(LoadedFont {
                font,
                request: gpui::font(
                    face.families
                        .first()
                        .map(|family| family.0.clone())
                        .unwrap_or_default(),
                ),
                features: CosmicFontFeatures::new(),
                is_known_emoji_font: check_is_known_emoji_font(&face.post_script_name),
                user_fallback_chain: Arc::from(Vec::new()),
            });

            Ok(font_id)
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        if contains_paragraph_separator(text) {
            self.layout_line_with_separators(text, font_size, font_runs)
        } else {
            self.layout_line_no_separators(text, font_size, font_runs)
        }
    }

    fn layout_line_with_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> LineLayout {
        let mut layout = LineLayout {
            font_size,
            len: text.len(),
            ..Default::default()
        };
        let mut paragraph_start = 0;

        for (separator_start, separator) in text
            .char_indices()
            .filter(|(_, character)| is_paragraph_separator(*character))
        {
            let separator_end = separator_start + separator.len_utf8();
            self.shape_segment(
                text,
                paragraph_start..separator_start,
                font_size,
                font_runs,
                &mut layout,
            );
            self.shape_segment(
                text,
                separator_start..separator_end,
                font_size,
                font_runs,
                &mut layout,
            );
            paragraph_start = separator_end;
        }

        self.shape_segment(
            text,
            paragraph_start..text.len(),
            font_size,
            font_runs,
            &mut layout,
        );

        layout
    }

    fn shape_segment(
        &mut self,
        text: &str,
        range: Range<usize>,
        font_size: Pixels,
        font_runs: &[FontRun],
        layout: &mut LineLayout,
    ) {
        if range.is_empty() {
            return;
        }

        let segment_font_runs = clip_font_runs(font_runs, range.clone());
        let segment =
            self.layout_line_no_separators(&text[range.clone()], font_size, &segment_font_runs);

        let mut segment_runs = segment.runs;
        for run in &mut segment_runs {
            for glyph in &mut run.glyphs {
                glyph.index += range.start;
                glyph.position.x += layout.width;
            }
        }

        for mut run in segment_runs {
            if let Some(same_run) = layout
                .runs
                .last_mut()
                .filter(|last| last.font_id == run.font_id)
            {
                same_run.glyphs.append(&mut run.glyphs);
            } else {
                layout.runs.push(run);
            }
        }

        layout.width += segment.width;
        layout.ascent = layout.ascent.max(segment.ascent);
        layout.descent = layout.descent.max(segment.descent);
    }

    fn layout_line_no_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> LineLayout {
        let mut attrs_list = AttrsList::new(&Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            let run_end = offs + run.len;

            let Some(properties) = self.prepare_font(run.font_id, &text[offs..run_end]) else {
                offs = run_end;
                continue;
            };

            let primary_attrs = properties.attributes(run.font_id, &properties.primary_family_name);
            let fallback_attrs: SmallVec<[Attrs<'_>; 4]> = properties
                .fallback_chain
                .iter()
                .map(|(font_id, family_name)| {
                    self.face_attributes(*font_id, family_name, &properties)
                })
                .collect();

            let spans = if properties.fallback_chain.is_empty() {
                let mut spans = SmallVec::<[RunSpan; 4]>::new();
                spans.push(RunSpan {
                    start: offs,
                    end: run_end,
                    slot: None,
                    font_id: run.font_id,
                });
                spans
            } else {
                let loaded_fonts = &self.loaded_fonts;
                let covers = |id: FontId, ch: char| charmap_covers(loaded_fonts, id, ch);
                compute_run_spans(
                    text,
                    offs,
                    run.len,
                    run.font_id,
                    &properties.fallback_chain,
                    &covers,
                )
            };

            for span in spans {
                let attrs = match span.slot {
                    None => &primary_attrs,
                    Some(ix) => &fallback_attrs[ix],
                };
                attrs_list.add_span(span.start..span.end, attrs);
            }
            offs = run_end;
        }

        let line = ShapeLine::new(
            &mut self.font_system,
            text,
            &attrs_list,
            cosmic_text::Shaping::Advanced,
            4,
        );
        let mut layout_lines = Vec::with_capacity(1);
        line.layout_to_buffer(
            &mut self.scratch,
            f32::from(font_size),
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            Ellipsize::None,
            None,
            &mut layout_lines,
            None,
            cosmic_text::Hinting::Disabled,
        );

        let Some(layout) = layout_lines.first() else {
            return LineLayout {
                font_size,
                width: Pixels::ZERO,
                ascent: Pixels::ZERO,
                descent: Pixels::ZERO,
                runs: Vec::new(),
                len: text.len(),
            };
        };

        let missing_glyphs = self.missing_glyph_sink.as_ref().map(|_| {
            self.missing_glyphs(
                text,
                font_runs,
                layout
                    .glyphs
                    .iter()
                    .filter(|glyph| glyph.glyph_id == 0)
                    .map(|glyph| glyph.start),
            )
        });

        let mut runs: Vec<ShapedRun> = Vec::new();
        for glyph in &layout.glyphs {
            let mut font_id = FontId(glyph.metadata);
            let mut loaded_font = self.loaded_font(font_id);
            if loaded_font.font.id() != glyph.font_id {
                match self.font_id_for_cosmic_id(glyph.font_id) {
                    std::result::Result::Ok(resolved_id) => {
                        font_id = resolved_id;
                        loaded_font = self.loaded_font(font_id);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to resolve cosmic font id {:?}: {error:#}",
                            glyph.font_id
                        );
                        continue;
                    }
                }
            }
            let is_emoji = loaded_font.is_known_emoji_font;

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && is_emoji {
                continue;
            }

            let shaped_glyph = ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(glyph.x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji,
            };

            if let Some(last_run) = runs
                .last_mut()
                .filter(|last_run| last_run.font_id == font_id)
            {
                last_run.glyphs.push(shaped_glyph);
            } else {
                runs.push(ShapedRun {
                    font_id,
                    glyphs: vec![shaped_glyph],
                });
            }
        }

        if let Some((sink, missing_glyphs)) = self.missing_glyph_sink.as_ref().zip(missing_glyphs) {
            sink.report(missing_glyphs);
        }

        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }

    fn missing_glyphs(
        &self,
        text: &str,
        font_runs: &[FontRun],
        missing_text_indices: impl IntoIterator<Item = usize>,
    ) -> Vec<MissingGlyph> {
        let mut missing_text_indices = missing_text_indices.into_iter().peekable();
        if missing_text_indices.peek().is_none() {
            return Vec::new();
        }
        let mut missing_text_indices = missing_text_indices.collect::<Vec<_>>();
        missing_text_indices.sort_unstable();
        missing_text_indices.dedup();

        let mut font_run_index = 0;
        let mut font_run_end = font_runs.first().map_or(0, |font_run| font_run.len);
        let mut missing_glyphs = Vec::new();
        let mut missing_index = 0;
        for (grapheme_start, grapheme) in text.grapheme_indices(true) {
            let grapheme_end = grapheme_start + grapheme.len();
            while missing_text_indices
                .get(missing_index)
                .is_some_and(|text_index| *text_index < grapheme_start)
            {
                missing_index += 1;
            }
            let Some(&text_index) = missing_text_indices.get(missing_index) else {
                break;
            };
            if text_index >= grapheme_end {
                continue;
            }

            while font_run_end <= text_index && font_run_index + 1 < font_runs.len() {
                font_run_index += 1;
                let Some(font_run) = font_runs.get(font_run_index) else {
                    break;
                };
                font_run_end += font_run.len;
            }
            let font_class = self.fallback_font_class(
                font_runs
                    .get(font_run_index)
                    .or_else(|| font_runs.last())
                    .map(|font_run| font_run.font_id),
            );
            missing_glyphs.push(MissingGlyph::new(grapheme.into(), font_class));
            while missing_text_indices
                .get(missing_index)
                .is_some_and(|text_index| *text_index < grapheme_end)
            {
                missing_index += 1;
            }
        }
        missing_glyphs
    }

    fn fallback_font_class(&self, font_id: Option<FontId>) -> FallbackFontClass {
        let Some(font_id) = font_id else {
            return FallbackFontClass::Proportional;
        };
        let loaded_font = self.loaded_font(font_id);
        let is_monospace = self
            .font_system
            .db()
            .face(loaded_font.font.id())
            .is_some_and(|face| face.monospaced);
        if is_monospace {
            FallbackFontClass::Monospace
        } else {
            FallbackFontClass::Proportional
        }
    }
}

#[inline(always)]
fn is_paragraph_separator(character: char) -> bool {
    unicode_bidi::bidi_class(character) == unicode_bidi::BidiClass::B
}

fn contains_paragraph_separator(text: &str) -> bool {
    if text
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0x1c | 0x1d | 0x1e))
    {
        return true;
    }

    !text.is_ascii() && text.chars().any(is_paragraph_separator)
}

fn clip_font_runs(font_runs: &[FontRun], range: Range<usize>) -> SmallVec<[FontRun; 4]> {
    let mut clipped = SmallVec::new();
    let mut offs = 0;
    for run in font_runs {
        let run_start = offs;
        offs += run.len;
        if offs <= range.start {
            continue;
        }
        if run_start >= range.end {
            break;
        }
        let start = run_start.max(range.start);
        let end = offs.min(range.end);
        if start < end {
            clipped.push(FontRun {
                len: end - start,
                font_id: run.font_id,
            });
        }
    }
    clipped
}

#[cfg(feature = "font-kit")]
fn find_best_match(
    font: &Font,
    candidates: &[cosmic_text::fontdb::ID],
    database: &cosmic_text::fontdb::Database,
) -> Result<usize> {
    let candidate_properties = candidates
        .iter()
        .map(|database_id| {
            let face_info = database
                .face(*database_id)
                .context("font face not found in database")?;
            Ok(face_info_into_properties(face_info))
        })
        .collect::<Result<SmallVec<[_; 4]>>>()?;

    let ix =
        font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
            .context("requested font family contains no font matching the other parameters")?;

    Ok(ix)
}

#[cfg(not(feature = "font-kit"))]
fn find_best_match(
    font: &Font,
    candidates: &[cosmic_text::fontdb::ID],
    database: &cosmic_text::fontdb::Database,
) -> Result<usize> {
    if candidates.is_empty() {
        anyhow::bail!("requested font family contains no font matching the other parameters");
    }
    if candidates.len() == 1 {
        return Ok(0);
    }

    let target_weight = font.weight.0;
    let target_italic = matches!(
        font.style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    );

    let mut best_index = 0;
    let mut best_score = u32::MAX;

    for (index, database_id) in candidates.iter().enumerate() {
        let face_info = database
            .face(*database_id)
            .context("font face not found in database")?;

        let is_italic = matches!(
            face_info.style,
            cosmic_text::Style::Italic | cosmic_text::Style::Oblique
        );
        let style_penalty: u32 = if is_italic == target_italic { 0 } else { 1000 };
        let weight_diff = (face_info.weight.0 as i32 - target_weight as i32).unsigned_abs();
        let score = style_penalty + weight_diff;

        if score < best_score {
            best_score = score;
            best_index = index;
        }
    }

    Ok(best_index)
}

/// one contiguous slice of a `FontRun` that maps to a single slot. `slot` is
/// `None` for the primary font and `Some(ix)` for `fallback_chain[ix]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunSpan {
    start: usize,
    end: usize,
    slot: Option<usize>,
    font_id: FontId,
}

/// walks `text[run_offset..run_offset + run_len]` and groups codepoints into
/// spans. inheriting codepoints stay in the current span so shaping clusters
/// like emoji zwj sequences and combining marks are not torn apart.
fn compute_run_spans(
    text: &str,
    run_offset: usize,
    run_len: usize,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> SmallVec<[RunSpan; 4]> {
    let mut spans = SmallVec::new();
    let run_end = run_offset + run_len;
    if run_end <= run_offset {
        return spans;
    }
    if fallback_chain.is_empty() {
        spans.push(RunSpan {
            start: run_offset,
            end: run_end,
            slot: None,
            font_id: primary,
        });
        return spans;
    }
    let run_text = &text[run_offset..run_end];
    let mut span_start = run_offset;
    let mut span_slot: Option<usize> = None;
    let mut span_font_id = primary;
    for (grapheme_idx, grapheme) in run_text.grapheme_indices(true) {
        let abs = run_offset + grapheme_idx;
        let covers_grapheme = |font_id| {
            grapheme
                .chars()
                .filter(|character| needs_glyph_coverage(*character))
                .all(|character| covers(font_id, character))
        };
        let next_slot = if covers_grapheme(primary) {
            None
        } else {
            fallback_chain
                .iter()
                .position(|(font_id, _)| covers_grapheme(*font_id))
                .or_else(|| {
                    // If no face covers the complete cluster, keep its base
                    // together and leave final fallback to cosmic-text.
                    let ch = grapheme.chars().next().unwrap_or('\0');
                    pick_covering_slot(ch, primary, fallback_chain, covers)
                })
        };
        if next_slot == span_slot {
            continue;
        }
        if abs > span_start {
            spans.push(RunSpan {
                start: span_start,
                end: abs,
                slot: span_slot,
                font_id: span_font_id,
            });
        }
        span_start = abs;
        span_slot = next_slot;
        span_font_id = slot_font_id(next_slot, primary, fallback_chain);
    }
    if span_start < run_end {
        spans.push(RunSpan {
            start: span_start,
            end: run_end,
            slot: span_slot,
            font_id: span_font_id,
        });
    }
    spans
}

fn slot_font_id(
    slot: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
) -> FontId {
    match slot {
        None => primary,
        Some(ix) => fallback_chain[ix].0,
    }
}

fn pick_covering_slot(
    ch: char,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> Option<usize> {
    if covers(primary, ch) {
        return None;
    }
    fallback_chain
        .iter()
        .position(|(fb_id, _)| covers(*fb_id, ch))
}

/// Joiners and variation selectors affect shaping without requiring separate
/// cmap glyphs. Keep them in the text, but do not reject a face for their absence.
fn needs_glyph_coverage(character: char) -> bool {
    !character.is_control()
        && !matches!(character, '\u{200c}' | '\u{200d}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

/// Keep whole missing graphemes for Fontconfig, but only track uncovered
/// required scalars when checking each newly loaded candidate.
#[cfg(target_os = "linux")]
fn missing_fallback_coverage(
    text: &str,
    mut uncovered: impl FnMut(char) -> bool,
) -> (String, Vec<char>) {
    let mut missing_text = String::new();
    let mut remaining = Vec::new();
    for grapheme in text.graphemes(true) {
        let previous_len = remaining.len();
        remaining.extend(grapheme.chars().filter(|&character| uncovered(character)));
        if remaining.len() != previous_len {
            missing_text.push_str(grapheme);
        }
    }
    remaining.sort_unstable();
    remaining.dedup();
    (missing_text, remaining)
}

fn charmap_covers(loaded_fonts: &[LoadedFont], id: FontId, ch: char) -> bool {
    loaded_fonts
        .get(id.0)
        .is_some_and(|loaded| loaded.font.as_swash().charmap().map(ch) != 0)
}

fn cosmic_font_features(features: &FontFeatures) -> Result<CosmicFontFeatures> {
    let mut result = CosmicFontFeatures::new();
    for feature in features.0.iter() {
        let name_bytes: [u8; 4] = feature
            .0
            .as_bytes()
            .try_into()
            .context("Incorrect feature flag format")?;

        let tag = cosmic_text::FeatureTag::new(&name_bytes);

        result.set(tag, feature.1);
    }
    Ok(result)
}

#[cfg(feature = "font-kit")]
fn font_into_properties(font: &gpui::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            gpui::FontStyle::Normal => font_kit::properties::Style::Normal,
            gpui::FontStyle::Italic => font_kit::properties::Style::Italic,
            gpui::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: Default::default(),
    }
}

#[cfg(feature = "font-kit")]
fn face_info_into_properties(
    face_info: &cosmic_text::fontdb::FaceInfo,
) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match face_info.style {
            cosmic_text::Style::Normal => font_kit::properties::Style::Normal,
            cosmic_text::Style::Italic => font_kit::properties::Style::Italic,
            cosmic_text::Style::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(face_info.weight.0.into()),
        stretch: match face_info.stretch {
            cosmic_text::Stretch::Condensed => font_kit::properties::Stretch::CONDENSED,
            cosmic_text::Stretch::Expanded => font_kit::properties::Stretch::EXPANDED,
            cosmic_text::Stretch::ExtraCondensed => font_kit::properties::Stretch::EXTRA_CONDENSED,
            cosmic_text::Stretch::ExtraExpanded => font_kit::properties::Stretch::EXTRA_EXPANDED,
            cosmic_text::Stretch::Normal => font_kit::properties::Stretch::NORMAL,
            cosmic_text::Stretch::SemiCondensed => font_kit::properties::Stretch::SEMI_CONDENSED,
            cosmic_text::Stretch::SemiExpanded => font_kit::properties::Stretch::SEMI_EXPANDED,
            cosmic_text::Stretch::UltraCondensed => font_kit::properties::Stretch::ULTRA_CONDENSED,
            cosmic_text::Stretch::UltraExpanded => font_kit::properties::Stretch::ULTRA_EXPANDED,
        },
    }
}

fn check_is_known_emoji_font(postscript_name: &str) -> bool {
    // TODO: Include other common emoji fonts
    postscript_name == "NotoColorEmoji"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "test-support")]
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn all_font_names_tracks_available_families() -> Result<()> {
        let text_system = gpui::TextSystem::new(Arc::new(
            CosmicTextSystem::new_without_system_fonts("IBM Plex Sans"),
        ));
        assert!(text_system.all_font_names().is_empty());

        text_system.add_fonts(vec![Cow::Borrowed(include_bytes!(
            "../../../tests/fixtures/fonts/lilex/Lilex-Regular.ttf"
        ))])?;
        assert_eq!(text_system.all_font_names(), ["Lilex"]);

        text_system.add_fonts(vec![
            Cow::Borrowed(IBM_PLEX),
            Cow::Borrowed(include_bytes!(
                "../../../tests/fixtures/fonts/lilex/Lilex-Bold.ttf"
            )),
        ])?;
        assert_eq!(text_system.all_font_names(), ["IBM Plex Sans", "Lilex"]);
        Ok(())
    }

    fn fid(i: usize) -> FontId {
        FontId(i)
    }

    fn chain(ids: &[usize]) -> SmallVec<[(FontId, SharedString); 4]> {
        ids.iter()
            .map(|&i| (fid(i), SharedString::from(format!("fb{i}"))))
            .collect()
    }

    fn span(start: usize, end: usize, slot: Option<usize>, font_id: FontId) -> RunSpan {
        RunSpan {
            start,
            end,
            slot,
            font_id,
        }
    }

    const IBM_PLEX: &[u8] =
        include_bytes!("../../../tests/fixtures/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
    const LILEX: &[u8] = include_bytes!("../../../tests/fixtures/fonts/lilex/Lilex-Regular.ttf");
    const LILEX_BOLD: &[u8] = include_bytes!("../../../tests/fixtures/fonts/lilex/Lilex-Bold.ttf");

    /// Every code point of `Bidi_Class=B`, each of which starts a new bidi
    /// paragraph and so can split one line into mixed-direction paragraphs.
    const SEPARATORS: &[char] = &[
        '\u{000a}', '\u{000d}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}', '\u{2029}',
    ];

    fn text_system() -> Result<CosmicTextSystem> {
        let text_system = CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        text_system.add_fonts(vec![Cow::Borrowed(IBM_PLEX)])?;
        Ok(text_system)
    }

    fn glyph_params(system: &CosmicTextSystem, ch: char) -> Result<RenderGlyphParams> {
        let font_id = system.font_id(&gpui::font("IBM Plex Sans"))?;
        Ok(RenderGlyphParams {
            font_id,
            glyph_id: system.glyph_for_char(font_id, ch).unwrap(),
            font_size: gpui::px(32.0),
            subpixel_variant: point(0, 0),
            scale_factor: 1.0,
            is_emoji: false,
            subpixel_rendering: false,
            dilation: 0,
        })
    }

    #[test]
    fn glyph_handoff_eviction_preserves_interleaved_rasterization() -> Result<()> {
        let system = text_system()?;
        let first = glyph_params(&system, 'A')?;
        let mut second = first.clone();
        second.font_size = gpui::px(48.0);
        second.subpixel_variant = point(1, 1);
        second.scale_factor = 1.5;
        second.subpixel_rendering = true;
        let mut expected = Vec::new();
        for params in [&first, &second] {
            let bounds = system.glyph_raster_bounds(params)?;
            expected.push((bounds, system.rasterize_glyph(params, bounds)?));
        }
        assert_eq!(system.glyph_raster_bounds(&first)?, expected[0].0);
        // A second window can ask for bounds before the first consumes its image.
        assert_eq!(system.glyph_raster_bounds(&second)?, expected[1].0);
        assert_eq!(
            system.rasterize_glyph(&first, expected[0].0)?,
            expected[0].1
        );
        assert_eq!(
            system.rasterize_glyph(&second, expected[1].0)?,
            expected[1].1
        );
        // A different glyph must not consume another request's bitmap either.
        let third = glyph_params(&system, 'B')?;
        system.glyph_raster_bounds(&third)?;
        assert_eq!(
            system.rasterize_glyph(&first, expected[0].0)?,
            expected[0].1
        );
        Ok(())
    }

    #[test]
    fn glyph_handoff_bounds_only_demand_keeps_retention_bounded() -> Result<()> {
        let system = text_system()?;
        let mut params = glyph_params(&system, 'W')?;
        params.subpixel_rendering = true;
        for i in 0..1024 {
            params.font_size = gpui::px(20.0 + i as f32 / 16.0);
            system.glyph_raster_bounds(&params)?;
            let state = system.0.read();
            let bytes = state
                .pending_glyph_image
                .as_ref()
                .map_or(0, |(_, image)| image.data.capacity());
            assert!(bytes <= MAX_PENDING_GLYPH_BYTES);
        }
        // Large glyphs remain supported, but cannot leave a large pending bitmap.
        params.font_size = gpui::px(1024.0);
        let bounds = system.glyph_raster_bounds(&params)?;
        assert!(system.0.read().pending_glyph_image.is_none());
        let (size, pixels) = system.rasterize_glyph(&params, bounds)?;
        assert_eq!(size, bounds.size);
        assert_eq!(
            pixels.len(),
            size.width.0 as usize * size.height.0 as usize * 4
        );
        assert!(pixels.len() > MAX_PENDING_GLYPH_BYTES);
        assert!(pixels.iter().any(|&value| value != 0));
        Ok(())
    }

    #[test]
    fn glyph_handoff_empty_and_failed_bounds_release_previous_bitmap() -> Result<()> {
        let system = text_system()?;
        let normal = glyph_params(&system, 'A')?;
        let space = glyph_params(&system, ' ')?;
        let expected_bounds = system.glyph_raster_bounds(&normal)?;
        let expected = system.rasterize_glyph(&normal, expected_bounds)?;
        system.glyph_raster_bounds(&normal)?;
        assert!(system.glyph_raster_bounds(&space)?.is_zero());
        assert!(system.0.read().pending_glyph_image.is_none());
        system.glyph_raster_bounds(&normal)?;
        let invalid = RenderGlyphParams {
            glyph_id: GlyphId(u32::MAX),
            ..normal.clone()
        };
        assert!(system.glyph_raster_bounds(&invalid).is_err());
        assert!(system.0.read().pending_glyph_image.is_none());
        let bounds = system.glyph_raster_bounds(&normal)?;
        assert_eq!(bounds, expected_bounds);
        assert_eq!(system.rasterize_glyph(&normal, bounds)?, expected);
        Ok(())
    }

    #[test]
    fn font_properties_describe_the_selected_face() -> Result<()> {
        let text_system = text_system()?;
        let regular = gpui::font("IBM Plex Sans");
        let regular_id = text_system.font_id(&regular)?;
        for (weight, style) in [
            (gpui::FontWeight::MEDIUM, gpui::FontStyle::Normal),
            (gpui::FontWeight::BOLD, gpui::FontStyle::Italic),
            (gpui::FontWeight::NORMAL, gpui::FontStyle::Oblique),
        ] {
            let requested = Font {
                weight,
                style,
                ..regular.clone()
            };
            let font_id = text_system.font_id(&requested)?;
            assert_eq!(font_id, regular_id);
            assert_eq!(
                text_system.font_weight_and_style(font_id)?,
                (gpui::FontWeight::NORMAL, gpui::FontStyle::Normal)
            );
        }
        Ok(())
    }

    #[test]
    fn loads_only_the_selected_face_and_reuses_requests() -> Result<()> {
        let text_system = CosmicTextSystem::new_without_system_fonts("Lilex");
        text_system.add_fonts(vec![Cow::Borrowed(LILEX), Cow::Borrowed(LILEX_BOLD)])?;
        let regular = gpui::font("Lilex");
        let regular_id = text_system.font_id(&regular)?;
        assert_eq!(text_system.0.read().loaded_fonts.len(), 1);
        let bold_id = text_system.font_id(&regular.clone().bold())?;
        assert_ne!(regular_id, bold_id);
        assert_eq!(text_system.0.read().loaded_fonts.len(), 2);
        assert_eq!(text_system.font_id(&regular)?, regular_id);
        assert_eq!(text_system.font_id(&regular.bold())?, bold_id);
        assert_eq!(text_system.0.read().loaded_fonts.len(), 2);
        assert_eq!(
            text_system.font_weight_and_style(bold_id)?,
            (gpui::FontWeight::BOLD, gpui::FontStyle::Normal),
        );
        Ok(())
    }

    #[test]
    fn explicit_fallbacks_match_request_and_never_overwrite_another_chain() -> Result<()> {
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Borrowed(LILEX), Cow::Borrowed(LILEX_BOLD)])?;
        let regular = Font {
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "missing family".into(),
                "Lilex".into(),
            ])),
            ..gpui::font("IBM Plex Sans")
        };
        let bold = regular.clone().bold();
        let regular_id = text_system.font_id(&regular)?;
        let bold_id = text_system.font_id(&bold)?;
        assert_ne!(regular_id, bold_id);
        let plain_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        assert_ne!(regular_id, plain_id);
        let mut state = text_system.0.write();
        let regular_chain = Arc::clone(&state.loaded_font(regular_id).user_fallback_chain);
        let bold_chain = Arc::clone(&state.loaded_font(bold_id).user_fallback_chain);
        assert_eq!(regular_chain.len(), 1);
        assert_eq!(bold_chain.len(), 1);
        assert_ne!(regular_chain[0].0, bold_chain[0].0);
        assert!(state.loaded_font(plain_id).user_fallback_chain.is_empty());
        let regular_face = state
            .font_system
            .db()
            .face(state.loaded_font(regular_chain[0].0).font.id())
            .unwrap();
        let bold_face = state
            .font_system
            .db()
            .face(state.loaded_font(bold_chain[0].0).font.id())
            .unwrap();
        assert_eq!(regular_face.weight, Weight::NORMAL);
        assert_eq!(bold_face.weight, Weight::BOLD);
        let properties = state.font_match_properties(bold_id).unwrap();
        assert_eq!(properties.weight, Weight::NORMAL);
        let fallback_attributes =
            state.face_attributes(bold_chain[0].0, &bold_chain[0].1, &properties);
        assert_eq!(fallback_attributes.weight, Weight::BOLD);
        assert_eq!(state.load_family(&regular, true)?, Some(regular_id));
        assert_eq!(
            &*state.loaded_font(regular_id).user_fallback_chain,
            &*regular_chain
        );
        assert_eq!(
            &*state.loaded_font(bold_id).user_fallback_chain,
            &*bold_chain
        );
        Ok(())
    }

    #[test]
    fn explicit_fallback_uses_matching_style_and_selected_stretch() -> Result<()> {
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Borrowed(LILEX)])?;
        let mut state = text_system.0.write();
        // Matching operates on metadata, so share the tracked face bytes while
        // representing normal and italic faces with a non-primary stretch.
        let mut normal = state
            .font_system
            .db()
            .faces()
            .find(|face| face.families.iter().any(|family| family.0 == "Lilex"))
            .unwrap()
            .clone();
        state.font_system.db_mut().remove_face(normal.id);
        normal.stretch = Stretch::SemiCondensed;
        let mut italic = normal.clone();
        italic.style = Style::Italic;
        state.font_system.db_mut().push_face_info(normal);
        let italic_id = state.font_system.db_mut().push_face_info(italic);
        let request = Font {
            fallbacks: Some(FontFallbacks::from_fonts(vec!["Lilex".into()])),
            ..gpui::font("IBM Plex Sans").italic()
        };
        let primary_id = state.load_family(&request, true)?.unwrap();
        let properties = state.font_match_properties(primary_id).unwrap();
        let (fallback_id, family) = &properties.fallback_chain[0];
        assert_eq!(state.loaded_font(*fallback_id).font.id(), italic_id);
        assert_eq!(properties.style, Style::Normal);
        assert_eq!(properties.stretch, Stretch::Normal);
        let attributes = state.face_attributes(*fallback_id, family, &properties);
        assert_eq!(attributes.style, Style::Italic);
        assert_eq!(attributes.stretch, Stretch::SemiCondensed);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fontconfig_file_mapping_registers_only_requested_face_metadata() -> Result<()> {
        let text_system = CosmicTextSystem::new_without_system_fonts("Lilex");
        let candidate = FontconfigCandidate {
            file: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/fonts/lilex/Lilex-Regular.ttf"),
            index: 0,
        };
        let mut state = text_system.0.write();
        let id = state.fontconfig_face_id(&candidate)?.unwrap();
        assert_eq!(state.font_system.db().faces().count(), 1);
        assert!(state.loaded_fonts.is_empty());
        let named_instance = FontconfigCandidate {
            index: 1 << 16,
            ..candidate
        };
        assert_eq!(state.fontconfig_face_id(&named_instance)?, Some(id));
        assert_eq!(state.font_system.db().faces().count(), 1);
        let font_id = state.load_face(id, &gpui::font("Lilex"), false)?.unwrap();
        assert_eq!(state.loaded_font(font_id).font.id(), id);
        assert_eq!(state.loaded_fonts.len(), 1);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_coverage_keeps_graphemes_and_tracks_only_required_scalars() {
        let (text, remaining) = missing_fallback_coverage(
            "a\u{301}a\u{301} \u{1f469}\u{200d}\u{1f4bb}\u{fe0f}\n",
            |character| needs_glyph_coverage(character) && character != 'a' && character != ' ',
        );
        assert_eq!(text, "a\u{301}a\u{301}\u{1f469}\u{200d}\u{1f4bb}\u{fe0f}");
        assert_eq!(remaining, ['\u{301}', '\u{1f469}', '\u{1f4bb}']);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_candidates_preserve_successes_across_mapping_and_loading_failures() -> Result<()> {
        let text_system = text_system()?;
        let primary = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let regular = FontconfigCandidate {
            file: root.join("../../tests/fixtures/fonts/lilex/Lilex-Regular.ttf"),
            index: 0,
        };
        let bold = FontconfigCandidate {
            file: root.join("../../tests/fixtures/fonts/lilex/Lilex-Bold.ttf"),
            index: 0,
        };
        // Neither path can be a file: its parent is the regular fixture file.
        let mapping_failure = FontconfigCandidate {
            file: regular.file.join("missing-metadata"),
            index: 0,
        };
        let loading_failure = FontconfigCandidate {
            file: regular.file.join("missing-font-data"),
            index: 0,
        };
        let mut state = text_system.0.write();
        let mut unloadable = state.font_system.db().faces().next().unwrap().clone();
        unloadable.source = cosmic_text::fontdb::Source::File(loading_failure.file.clone());
        state.font_system.db_mut().push_face_info(unloadable);
        let mut properties = state.font_match_properties(primary).unwrap();
        let request = gpui::font("IBM Plex Sans");
        let mut remaining = vec!['m', '\u{10ffff}'];
        state.append_system_fallbacks(
            primary,
            &request,
            &[regular.clone(), mapping_failure, loading_failure, bold.clone()],
            &mut remaining,
            &mut properties,
        );
        assert_eq!(remaining, ['\u{10ffff}']);
        assert_eq!(properties.fallback_chain.len(), 2);
        let regular_id = state.fontconfig_face_id(&regular)?.unwrap();
        let bold_id = state.fontconfig_face_id(&bold)?.unwrap();
        assert_eq!(state.loaded_font(properties.fallback_chain[0].0).font.id(), regular_id);
        assert_eq!(state.loaded_font(properties.fallback_chain[1].0).font.id(), bold_id);

        // A covered requirement stops before loading later ordered candidates.
        let mut properties = state.font_match_properties(primary).unwrap();
        let mut remaining = vec!['m'];
        state.append_system_fallbacks(
            primary,
            &request,
            &[regular, bold],
            &mut remaining,
            &mut properties,
        );
        assert!(remaining.is_empty());
        assert_eq!(properties.fallback_chain.len(), 1);
        assert_eq!(state.loaded_font(properties.fallback_chain[0].0).font.id(), regular_id);
        Ok(())
    }

    #[test]
    fn features_keep_distinct_loaded_entries() -> Result<()> {
        let text_system = text_system()?;
        let regular = gpui::font("IBM Plex Sans");
        let plain_id = text_system.font_id(&regular)?;
        let no_ligatures = Font {
            features: FontFeatures::disable_ligatures(),
            ..regular.clone()
        };
        let feature_id = text_system.font_id(&no_ligatures)?;
        assert_ne!(plain_id, feature_id);
        let state = text_system.0.read();
        assert_eq!(
            state.loaded_font(plain_id).font.id(),
            state.loaded_font(feature_id).font.id()
        );
        assert_eq!(
            state.loaded_font(plain_id).features,
            cosmic_font_features(&regular.features)?
        );
        assert_eq!(
            state.loaded_font(feature_id).features,
            cosmic_font_features(&no_ligatures.features)?
        );
        Ok(())
    }

    #[test]
    fn fallback_only_face_does_not_require_primary_latin_metrics() -> Result<()> {
        // Make the tracked fixture's cmap empty, retaining its real metadata,
        // metrics, and outlines. No external or installed font is needed.
        let mut no_latin = LILEX.to_vec();
        let table_count = u16::from_be_bytes(no_latin[4..6].try_into().unwrap()) as usize;
        let cmap_offset = no_latin[12..12 + table_count * 16]
            .chunks_exact(16)
            .find(|record| &record[..4] == b"cmap")
            .map(|record| u32::from_be_bytes(record[8..12].try_into().unwrap()) as usize)
            .unwrap();
        no_latin[cmap_offset + 2..cmap_offset + 4].copy_from_slice(&0u16.to_be_bytes());
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Owned(no_latin)])?;
        let mut state = text_system.0.write();
        let fallback = gpui::font("Lilex");
        let fallback_id = state.load_family(&fallback, false)?.unwrap();
        assert!(!charmap_covers(&state.loaded_fonts, fallback_id, 'm'));
        assert_eq!(state.load_family(&fallback, true)?, None);
        let primary = Font {
            fallbacks: Some(FontFallbacks::from_fonts(vec!["Lilex".into()])),
            ..gpui::font("IBM Plex Sans")
        };
        let primary_id = state.load_family(&primary, true)?.unwrap();
        assert!(charmap_covers(&state.loaded_fonts, primary_id, 'm'));
        assert_eq!(
            state.loaded_font(primary_id).user_fallback_chain[0].0,
            fallback_id
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_fallback_is_lazy_and_keeps_source_style_identity() -> Result<()> {
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Borrowed(LILEX)])?;
        let descriptor = FontFallbacks::from_system_family("sans-serif", "IBM Plex Sans");
        let regular = Font {
            fallbacks: Some(descriptor.clone()),
            ..gpui::font("IBM Plex Sans")
        };
        let regular_id = text_system.font_id(&regular)?;
        let bold_id = text_system.font_id(&regular.bold())?;
        assert_ne!(regular_id, bold_id);
        let independent = Font {
            fallbacks: Some(descriptor),
            ..gpui::font("Lilex")
        };
        let independent_id = text_system.font_id(&independent)?;
        let mut state = text_system.0.write();
        assert!(state.fontconfig.is_none());
        assert_eq!(
            state.loaded_font(bold_id).request.weight,
            gpui::FontWeight::BOLD
        );
        assert!(state.loaded_font(regular_id).user_fallback_chain.is_empty());
        assert!(state.prepare_font(regular_id, "readable Latin").is_some());
        // An icon-family override inherits the descriptor but not system fallback.
        assert!(state.prepare_font(independent_id, "\u{10ffff}").is_some());
        assert!(state.fontconfig.is_none());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn primary_and_fallback_preserve_the_same_initialization_failure() -> Result<()> {
        let backend = Arc::new(text_system()?);
        backend.0.write().fontconfig = Some(Err("isolated configuration failure".into()));
        let text_system = gpui::TextSystem::new(backend.clone());
        for _ in 0..2 {
            let error = text_system.resolve_system_font_family("monospace").unwrap_err();
            assert_eq!(error.to_string(), "isolated configuration failure");
        }

        let request = Font {
            fallbacks: Some(FontFallbacks::from_system_family("monospace", "IBM Plex Sans")),
            ..gpui::font("IBM Plex Sans")
        };
        let primary = backend.font_id(&request)?;
        let mut state = backend.0.write();
        let mut properties = state.font_match_properties(primary).unwrap();
        let error = state
            .prepare_system_fallbacks(primary, "\u{10ffff}", &mut properties)
            .unwrap_err();
        assert_eq!(error.to_string(), "isolated configuration failure");
        assert!(properties.fallback_chain.is_empty());
        Ok(())
    }

    #[test]
    fn complete_grapheme_fallback_precedes_partially_covering_primary() {
        let fb = chain(&[1]);
        for (text, base, missing) in [
            ("a\u{301}", 'a', '\u{301}'),
            ("a\u{301}\u{fe0f}", 'a', '\u{301}'),
            ("\u{1f469}\u{200d}\u{1f4bb}", '\u{1f469}', '\u{1f4bb}'),
        ] {
            let covers = |id: FontId, ch: char| {
                (id == fid(0) && ch == base) || (id == fid(1) && (ch == base || ch == missing))
            };
            assert_eq!(
                compute_run_spans(text, 0, text.len(), fid(0), &fb, &covers),
                [span(0, text.len(), Some(0), fid(1))]
                    .into_iter()
                    .collect::<SmallVec<[RunSpan; 4]>>(),
                "{text:?}",
            );
        }
    }

    #[test]
    fn ordered_fallbacks_do_not_prefer_a_previous_later_face() {
        let fb = chain(&[1, 2]);
        let covers = |id: FontId, ch: char| {
            (id == fid(2) && ch == '字') || ((id == fid(1) || id == fid(2)) && ch == '体')
        };
        assert_eq!(
            compute_run_spans("字体", 0, "字体".len(), fid(0), &fb, &covers),
            [span(0, 3, Some(1), fid(2)), span(3, 6, Some(0), fid(1))]
                .into_iter()
                .collect::<SmallVec<[RunSpan; 4]>>(),
        );
        assert_eq!(
            pick_covering_slot('?', fid(0), &fb, &|id, _| id == fid(1)),
            Some(0)
        );
    }

    fn layout_text(text_system: &CosmicTextSystem, text: &str) -> Result<LineLayout> {
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];
        Ok(text_system.layout_line(text, gpui::px(14.0), &runs))
    }

    /// Mirrors the original crash: mixed-direction text reaching the shaper
    /// through `shape_text`, which only splits lines on `\n`.
    #[test]
    fn shape_text_with_mixed_direction_paragraphs() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let text_system = Arc::new(gpui::TextSystem::new(platform_text_system));
        let window_text_system = gpui::WindowTextSystem::new(text_system);

        let text: SharedString = "first line\n\u{05d0}\u{001c}A".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];

        let lines = window_text_system.shape_text(text, gpui::px(14.0), &runs, None, None)?;

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].len(), "\u{05d0}\u{001c}A".len());
        assert!(lines[1].width() > Pixels::ZERO);
        Ok(())
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn reports_graphemes_that_exhaust_font_fallback() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let dispatcher = gpui::TestDispatcher::new(0);
        let cx =
            gpui::TestAppContext::build_with_text_system(dispatcher, None, platform_text_system);
        let observed = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let observed = observed.clone();
            cx.on_missing_glyphs(move |missing_glyphs, _| {
                observed.borrow_mut().extend_from_slice(missing_glyphs);
            })
        });
        let text: SharedString = "界".into();

        cx.update(|cx| {
            let text_system = gpui::WindowTextSystem::new(cx.text_system().clone());
            let runs = [gpui::TextRun {
                len: text.len(),
                font: gpui::font("IBM Plex Sans"),
                ..Default::default()
            }];
            text_system.shape_line(text, gpui::px(14.0), &runs, None);
        });
        cx.run_until_parked();

        let observed = observed.borrow();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].grapheme(), "界");
        assert_eq!(
            observed[0].font_class(),
            gpui::FallbackFontClass::Proportional
        );
        Ok(())
    }

    #[test]
    fn combines_missing_glyphs_from_one_grapheme() -> Result<()> {
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "x\u{0301}";
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];

        let missing_glyphs = text_system
            .0
            .read()
            .missing_glyphs(text, &runs, [0, "x".len()]);

        assert_eq!(missing_glyphs.len(), 1);
        assert_eq!(missing_glyphs[0].grapheme(), text);
        Ok(())
    }

    #[test]
    fn adding_fonts_invalidates_cached_line_layouts() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let text_system = Arc::new(gpui::TextSystem::new(platform_text_system.clone()));
        let window_text_system = gpui::WindowTextSystem::new(text_system.clone());
        let text: SharedString = "cached text".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];

        let first_layout = window_text_system.shape_line(text.clone(), gpui::px(14.0), &runs, None);
        let cached_layout =
            window_text_system.shape_line(text.clone(), gpui::px(14.0), &runs, None);
        assert!(std::ptr::eq::<LineLayout>(
            &**first_layout,
            &**cached_layout
        ));
        let loaded_font_count = platform_text_system.0.read().loaded_fonts.len();

        text_system.add_fonts(vec![Cow::Borrowed(LILEX)])?;

        let refreshed_layout = window_text_system.shape_line(text, gpui::px(14.0), &runs, None);
        assert!(!std::ptr::eq::<LineLayout>(
            &**first_layout,
            &**refreshed_layout
        ));
        assert_eq!(
            platform_text_system.0.read().loaded_fonts.len(),
            loaded_font_count
        );
        Ok(())
    }

    #[test]
    fn layout_line_with_mixed_direction_paragraphs() -> Result<()> {
        let text_system = text_system()?;

        for separator in SEPARATORS {
            for text in [
                format!("\u{05d0}{separator}A"),
                format!("A{separator}\u{05d0}"),
            ] {
                let layout = layout_text(&text_system, &text)?;

                assert_eq!(layout.len, text.len(), "{text:?}");
                assert!(layout.width > Pixels::ZERO, "{text:?}");
                assert!(
                    layout.runs.iter().any(|run| !run.glyphs.is_empty()),
                    "{text:?}"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn layout_line_with_separators_at_line_edges() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "\u{001c}",
            "\u{001c}\u{001c}",
            "\u{001c}\u{05d0}",
            "\u{05d0}\u{001c}",
            "\u{05d0}\u{001c}\u{001c}A",
            "\u{001c}\u{05d0}\u{001c}A\u{001c}",
        ] {
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
        }

        Ok(())
    }

    /// Glyph indices must stay absolute and positions ordered across segment
    /// boundaries, otherwise cursor placement and hit testing desync. Uses
    /// single-direction text so visual order matches logical order.
    #[test]
    fn layout_line_keeps_indices_and_positions_ordered_across_paragraphs() -> Result<()> {
        let text_system = text_system()?;
        let text = "ab\u{001c}cd\u{2029}ef";
        let layout = layout_text(&text_system, text)?;

        let glyphs: Vec<_> = layout.runs.iter().flat_map(|run| &run.glyphs).collect();
        assert!(!glyphs.is_empty());

        for glyph in &glyphs {
            assert!(glyph.index < text.len(), "{:?}", glyph.index);
            assert!(text.is_char_boundary(glyph.index), "{:?}", glyph.index);
        }
        for pair in glyphs.windows(2) {
            assert!(pair[0].index < pair[1].index);
            assert!(pair[0].position.x <= pair[1].position.x);
        }

        // Every segment contributes width, so the whole line is wider than its
        // leading paragraph alone.
        assert!(layout.width > layout_text(&text_system, "ab")?.width);
        Ok(())
    }

    /// A font run boundary that does not line up with a paragraph boundary must
    /// still be clipped to the right segments.
    #[test]
    fn layout_line_with_font_run_straddling_a_separator() -> Result<()> {
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "ab\u{001c}\u{05d0}\u{05d1}";

        // The run boundary falls inside the trailing RTL paragraph.
        let runs = [
            FontRun {
                len: "ab\u{001c}\u{05d0}".len(),
                font_id,
            },
            FontRun {
                len: "\u{05d1}".len(),
                font_id,
            },
        ];
        let layout = text_system.layout_line(text, gpui::px(14.0), &runs);

        assert_eq!(layout.len, text.len());
        assert!(layout.width > Pixels::ZERO);
        Ok(())
    }

    /// Lines with no separator take the fast path and must be shaped exactly as
    /// they were before paragraph splitting existed.
    #[test]
    fn layout_line_without_separators_takes_fast_path() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "hello world",
            "\u{05d0}\u{05d1}\u{05d2}",
            "mixed \u{05d0}\u{05d1}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
            assert!(layout.width > Pixels::ZERO, "{text:?}");
        }

        Ok(())
    }

    /// cosmic-text sums word widths to get a line's width but accumulates glyph
    /// advances to position glyphs, so a trailing zero-advance glyph (here a
    /// zero-width space) can land a few ulps past the width. When that glyph is a
    /// wrap boundary, the row before it extends past the line's width, and hit
    /// testing in that sliver used to panic (ZED-BW8, ZED-75K, ZED-81Z).
    #[test]
    fn index_for_position_past_line_width() -> Result<()> {
        let text_system = Arc::new(gpui::TextSystem::new(Arc::new(text_system()?)));
        let window_text_system = gpui::WindowTextSystem::new(text_system);
        let text: SharedString = "Warning: this will delete files\u{200b}".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];
        let lines =
            window_text_system.shape_text(text, gpui::px(14.), &runs, Some(gpui::px(4.)), None)?;
        let line = &lines[0];
        let width = line.unwrapped_layout.width;
        let boundary_glyph = |row: usize| {
            let boundary = line.wrap_boundaries()[row];
            &line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix]
        };
        let row = (1..line.wrap_boundaries().len())
            .find(|row| boundary_glyph(*row).position.x > width)
            .expect("trailing zero-width space should be a wrap boundary past the line width");
        let row_start_x = f32::from(boundary_glyph(row - 1).position.x);
        let row_end = boundary_glyph(row);

        let mut x = f32::from(width) - row_start_x;
        while x + row_start_x < f32::from(width) {
            x = x.next_up();
        }
        assert!(gpui::px(x + row_start_x) < row_end.position.x);

        let line_height = gpui::px(20.);
        let position = gpui::point(gpui::px(x), line_height * row as f32 + gpui::px(1.));
        assert_eq!(
            line.index_for_position(position, line_height),
            Err(row_end.index)
        );
        Ok(())
    }

    #[test]
    fn paragraph_separator_detection() {
        for separator in SEPARATORS {
            assert!(is_paragraph_separator(*separator), "{separator:?}");
            assert!(contains_paragraph_separator(&format!("a{separator}b")));
        }

        for text in [
            "",
            "plain ascii",
            "\u{05d0}",
            "tab\there",
            "emoji \u{1f600}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
        }
    }

    #[test]
    fn font_runs_are_clipped_to_segment() {
        let runs = [
            FontRun {
                len: 3,
                font_id: fid(1),
            },
            FontRun {
                len: 4,
                font_id: fid(2),
            },
        ];

        assert_eq!(clip_font_runs(&runs, 0..7).as_slice(), &runs);
        assert_eq!(
            clip_font_runs(&runs, 2..5).as_slice(),
            &[
                FontRun {
                    len: 1,
                    font_id: fid(1)
                },
                FontRun {
                    len: 2,
                    font_id: fid(2)
                },
            ]
        );
        assert_eq!(
            clip_font_runs(&runs, 3..7).as_slice(),
            &[FontRun {
                len: 4,
                font_id: fid(2)
            }]
        );
        assert!(clip_font_runs(&runs, 5..5).is_empty());
    }

    #[test]
    fn primary_wins_when_primary_covers() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |id: FontId, _: char| id == fid(0) || id == fid(1);
        assert_eq!(pick_covering_slot('a', primary, &fb, &covers), None);
    }

    #[test]
    fn primary_preferred_over_fallback_when_both_cover() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        assert_eq!(pick_covering_slot('a', primary, &fb, &covers), None);
    }

    #[test]
    fn falls_through_chain_in_order() {
        let primary = fid(0);
        let fb = chain(&[1, 2, 3]);
        // only fallback 2 at index 1 covers.
        let covers = |id: FontId, _: char| id == fid(2);
        assert_eq!(pick_covering_slot('字', primary, &fb, &covers), Some(1));
    }

    #[test]
    fn no_coverage_returns_primary() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |_: FontId, _: char| false;
        // nothing covers. return `None` so the `cosmic-text` built in script
        // fallback can take over during shaping.
        assert_eq!(pick_covering_slot('\u{1F600}', primary, &fb, &covers), None);
    }

    #[test]
    fn empty_chain_always_returns_primary() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        assert_eq!(pick_covering_slot('a', primary, &fb, &covers), None);
    }

    #[test]
    fn slot_font_id_resolution() {
        let primary = fid(7);
        let fb = chain(&[10, 20]);
        assert_eq!(slot_font_id(None, primary, &fb), fid(7));
        assert_eq!(slot_font_id(Some(0), primary, &fb), fid(10));
        assert_eq!(slot_font_id(Some(1), primary, &fb), fid(20));
    }

    #[test]
    fn run_spans_with_no_chain_emit_one_primary_span() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        let text = "hello";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), None, primary)]);
    }

    #[test]
    fn run_spans_use_byte_offsets_for_multibyte_chars() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii. fallback covers cjk.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "a字b";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        // '字' is 3 bytes so split is at 1 then 4.
        assert_eq!(
            spans.as_slice(),
            &[
                span(0, 1, None, primary),
                span(1, 4, Some(0), fid(1)),
                span(4, 5, None, primary),
            ]
        );
    }

    #[test]
    fn run_spans_respect_run_offset() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        // outer text has a prefix that is not part of this run.
        let text = "xx字y";
        let run_offset = 2;
        let run_len = text.len() - run_offset;
        let spans = compute_run_spans(text, run_offset, run_len, primary, &fb, &covers);
        assert_eq!(
            spans.as_slice(),
            &[span(2, 5, Some(0), fid(1)), span(5, 6, None, primary)]
        );
    }

    #[test]
    fn run_spans_keep_combining_marks_with_base_in_fallback() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii only. fallback covers the base char.
        // combining mark must stay in the fallback span even when fallback
        // does not advertise coverage of it.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                ch == '\u{0905}'
            }
        };
        // \u{0905} devanagari short a + \u{0902} candrabindu mark.
        let text = "\u{0905}\u{0902}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_keep_zwj_inside_emoji_cluster() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // only fallback covers the emoji codepoints. zwj must not split.
        let covers = |id: FontId, ch: char| id == fid(1) && ch != '\u{200D}';
        // family zwj sequence woman zwj girl.
        let text = "\u{1F469}\u{200D}\u{1F467}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_collapse_adjacent_same_slot() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "字字字";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_empty_run_returns_no_spans() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        let spans = compute_run_spans("anything", 3, 0, primary, &fb, &covers);
        assert!(spans.is_empty());
    }
}
