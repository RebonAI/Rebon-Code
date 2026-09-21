/// The six selectable themes, in the same order as [`THEME_NAMES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeName {
    /// `dark`, the default.
    Dark,
    /// `light`.
    Light,
    /// `light-daltonized`, safe for red/green color blindness.
    LightDaltonized,
    /// `dark-daltonized`, safe for red/green color blindness.
    DarkDaltonized,
    /// `light-ansi`, a 16-color fallback for limited terminals.
    LightAnsi,
    /// `dark-ansi`, a 16-color fallback for limited terminals.
    DarkAnsi,
}

/// Every theme name paired with its variant, in canonical order.
pub const THEME_NAMES: &[(&str, ThemeName)] = &[
    ("dark", ThemeName::Dark),
    ("light", ThemeName::Light),
    ("light-daltonized", ThemeName::LightDaltonized),
    ("dark-daltonized", ThemeName::DarkDaltonized),
    ("light-ansi", ThemeName::LightAnsi),
    ("dark-ansi", ThemeName::DarkAnsi),
];

impl ThemeName {
    /// Parse a theme name from its config string. Anything that is not one
    /// of the six known names — including a differently-cased spelling —
    /// yields `None`.
    pub fn from_str(name: &str) -> Option<ThemeName> {
        for (n, kind) in THEME_NAMES {
            if *n == name {
                return Some(*kind);
            }
        }
        None
    }

    /// The config string for this theme.
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::LightDaltonized => "light-daltonized",
            ThemeName::DarkDaltonized => "dark-daltonized",
            ThemeName::LightAnsi => "light-ansi",
            ThemeName::DarkAnsi => "dark-ansi",
        }
    }
}

impl Default for ThemeName {
    /// The default theme is `Dark`.
    fn default() -> Self {
        ThemeName::Dark
    }
}

/// What the user chose for their theme: either `auto` or a concrete
/// [`ThemeName`].
///
/// `Auto` is not resolved here; turning it into a `ThemeName` needs the
/// consumer's own dark/light detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeSetting {
    /// `auto` — follow the system's dark/light mode.
    Auto,
    /// An explicitly chosen theme.
    Named(ThemeName),
}

/// Every valid theme-setting string, with `auto` first.
pub const THEME_SETTINGS: &[&str] = &[
    "auto",
    "dark",
    "light",
    "light-daltonized",
    "dark-daltonized",
    "light-ansi",
    "dark-ansi",
];

/// One color per theme key.
///
/// Every field is a `&'static str`, which is what lets the six palettes be
/// `pub const`s rather than values assembled at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_snake_case)]
pub struct Theme {
    pub autoAccept: &'static str,
    pub bashBorder: &'static str,
    pub rebon: &'static str,
    pub rebonShimmer: &'static str,
    pub rebonBlue_FOR_SYSTEM_SPINNER: &'static str,
    pub rebonBlueShimmer_FOR_SYSTEM_SPINNER: &'static str,
    pub permission: &'static str,
    pub permissionShimmer: &'static str,
    pub planMode: &'static str,
    pub ide: &'static str,
    pub promptBorder: &'static str,
    pub promptBorderShimmer: &'static str,
    pub text: &'static str,
    pub inverseText: &'static str,
    pub inactive: &'static str,
    pub inactiveShimmer: &'static str,
    pub subtle: &'static str,
    pub suggestion: &'static str,
    pub remember: &'static str,
    pub background: &'static str,
    pub success: &'static str,
    pub error: &'static str,
    pub warning: &'static str,
    pub merged: &'static str,
    pub warningShimmer: &'static str,
    pub diffAdded: &'static str,
    pub diffRemoved: &'static str,
    pub diffAddedDimmed: &'static str,
    pub diffRemovedDimmed: &'static str,
    pub diffAddedWord: &'static str,
    pub diffRemovedWord: &'static str,
    pub red_FOR_SUBAGENTS_ONLY: &'static str,
    pub blue_FOR_SUBAGENTS_ONLY: &'static str,
    pub green_FOR_SUBAGENTS_ONLY: &'static str,
    pub yellow_FOR_SUBAGENTS_ONLY: &'static str,
    pub purple_FOR_SUBAGENTS_ONLY: &'static str,
    pub orange_FOR_SUBAGENTS_ONLY: &'static str,
    pub pink_FOR_SUBAGENTS_ONLY: &'static str,
    pub cyan_FOR_SUBAGENTS_ONLY: &'static str,
    pub professionalBlue: &'static str,
    pub chromeYellow: &'static str,
    pub rebonMascotBody: &'static str,
    pub rebonMascotBackground: &'static str,
    pub userMessageBackground: &'static str,
    pub userMessageBackgroundHover: &'static str,
    pub messageActionsBackground: &'static str,
    pub selectionBg: &'static str,
    pub bashMessageBackgroundColor: &'static str,
    pub memoryBackgroundColor: &'static str,
    pub rate_limit_fill: &'static str,
    pub rate_limit_empty: &'static str,
    pub fastMode: &'static str,
    pub fastModeShimmer: &'static str,
    pub briefLabelYou: &'static str,
    pub briefLabelRebon: &'static str,
    pub rainbow_red: &'static str,
    pub rainbow_orange: &'static str,
    pub rainbow_yellow: &'static str,
    pub rainbow_green: &'static str,
    pub rainbow_blue: &'static str,
    pub rainbow_indigo: &'static str,
    pub rainbow_violet: &'static str,
    pub rainbow_red_shimmer: &'static str,
    pub rainbow_orange_shimmer: &'static str,
    pub rainbow_yellow_shimmer: &'static str,
    pub rainbow_green_shimmer: &'static str,
    pub rainbow_blue_shimmer: &'static str,
    pub rainbow_indigo_shimmer: &'static str,
    pub rainbow_violet_shimmer: &'static str,
}

impl Theme {
    /// Look a color up by theme key. Keys are this struct's field names;
    /// anything else returns `None`.
    ///
    /// This is the bridge [`crate::color::resolve_color`] uses to turn a
    /// configuration key into a literal.
    pub fn lookup(&self, key: &str) -> Option<&'static str> {
        let v = match key {
            "autoAccept" => self.autoAccept,
            "bashBorder" => self.bashBorder,
            "rebon" => self.rebon,
            "rebonShimmer" => self.rebonShimmer,
            "rebonBlue_FOR_SYSTEM_SPINNER" => self.rebonBlue_FOR_SYSTEM_SPINNER,
            "rebonBlueShimmer_FOR_SYSTEM_SPINNER" => self.rebonBlueShimmer_FOR_SYSTEM_SPINNER,
            "permission" => self.permission,
            "permissionShimmer" => self.permissionShimmer,
            "planMode" => self.planMode,
            "ide" => self.ide,
            "promptBorder" => self.promptBorder,
            "promptBorderShimmer" => self.promptBorderShimmer,
            "text" => self.text,
            "inverseText" => self.inverseText,
            "inactive" => self.inactive,
            "inactiveShimmer" => self.inactiveShimmer,
            "subtle" => self.subtle,
            "suggestion" => self.suggestion,
            "remember" => self.remember,
            "background" => self.background,
            "success" => self.success,
            "error" => self.error,
            "warning" => self.warning,
            "merged" => self.merged,
            "warningShimmer" => self.warningShimmer,
            "diffAdded" => self.diffAdded,
            "diffRemoved" => self.diffRemoved,
            "diffAddedDimmed" => self.diffAddedDimmed,
            "diffRemovedDimmed" => self.diffRemovedDimmed,
            "diffAddedWord" => self.diffAddedWord,
            "diffRemovedWord" => self.diffRemovedWord,
            "red_FOR_SUBAGENTS_ONLY" => self.red_FOR_SUBAGENTS_ONLY,
            "blue_FOR_SUBAGENTS_ONLY" => self.blue_FOR_SUBAGENTS_ONLY,
            "green_FOR_SUBAGENTS_ONLY" => self.green_FOR_SUBAGENTS_ONLY,
            "yellow_FOR_SUBAGENTS_ONLY" => self.yellow_FOR_SUBAGENTS_ONLY,
            "purple_FOR_SUBAGENTS_ONLY" => self.purple_FOR_SUBAGENTS_ONLY,
            "orange_FOR_SUBAGENTS_ONLY" => self.orange_FOR_SUBAGENTS_ONLY,
            "pink_FOR_SUBAGENTS_ONLY" => self.pink_FOR_SUBAGENTS_ONLY,
            "cyan_FOR_SUBAGENTS_ONLY" => self.cyan_FOR_SUBAGENTS_ONLY,
            "professionalBlue" => self.professionalBlue,
            "chromeYellow" => self.chromeYellow,
            "rebonMascotBody" => self.rebonMascotBody,
            "rebonMascotBackground" => self.rebonMascotBackground,
            "userMessageBackground" => self.userMessageBackground,
            "userMessageBackgroundHover" => self.userMessageBackgroundHover,
            "messageActionsBackground" => self.messageActionsBackground,
            "selectionBg" => self.selectionBg,
            "bashMessageBackgroundColor" => self.bashMessageBackgroundColor,
            "memoryBackgroundColor" => self.memoryBackgroundColor,
            "rate_limit_fill" => self.rate_limit_fill,
            "rate_limit_empty" => self.rate_limit_empty,
            "fastMode" => self.fastMode,
            "fastModeShimmer" => self.fastModeShimmer,
            "briefLabelYou" => self.briefLabelYou,
            "briefLabelRebon" => self.briefLabelRebon,
            "rainbow_red" => self.rainbow_red,
            "rainbow_orange" => self.rainbow_orange,
            "rainbow_yellow" => self.rainbow_yellow,
            "rainbow_green" => self.rainbow_green,
            "rainbow_blue" => self.rainbow_blue,
            "rainbow_indigo" => self.rainbow_indigo,
            "rainbow_violet" => self.rainbow_violet,
            "rainbow_red_shimmer" => self.rainbow_red_shimmer,
            "rainbow_orange_shimmer" => self.rainbow_orange_shimmer,
            "rainbow_yellow_shimmer" => self.rainbow_yellow_shimmer,
            "rainbow_green_shimmer" => self.rainbow_green_shimmer,
            "rainbow_blue_shimmer" => self.rainbow_blue_shimmer,
            "rainbow_indigo_shimmer" => self.rainbow_indigo_shimmer,
            "rainbow_violet_shimmer" => self.rainbow_violet_shimmer,
            _ => return None,
        };
        Some(v)
    }
}

/// The `light` palette: RGB literals tuned for a light background.
#[allow(non_upper_case_globals)]
pub const LIGHT_THEME: Theme = Theme {
    autoAccept: "rgb(135,0,255)",
    bashBorder: "rgb(255,0,135)",
    rebon: "rgb(70,117,164)",
    rebonShimmer: "rgb(47,99,151)",
    rebonBlue_FOR_SYSTEM_SPINNER: "rgb(70,117,164)",
    rebonBlueShimmer_FOR_SYSTEM_SPINNER: "rgb(47,99,151)",
    permission: "rgb(87,105,247)",
    permissionShimmer: "rgb(137,155,255)",
    planMode: "rgb(0,102,102)",
    ide: "rgb(71,130,200)",
    promptBorder: "rgb(153,153,153)",
    promptBorderShimmer: "rgb(183,183,183)",
    text: "rgb(0,0,0)",
    inverseText: "rgb(255,255,255)",
    inactive: "rgb(102,102,102)",
    inactiveShimmer: "rgb(142,142,142)",
    subtle: "rgb(175,175,175)",
    suggestion: "rgb(87,105,247)",
    remember: "rgb(0,0,255)",
    background: "rgb(0,153,153)",
    success: "rgb(44,122,57)",
    error: "rgb(171,43,63)",
    warning: "rgb(150,108,30)",
    merged: "rgb(135,0,255)",
    warningShimmer: "rgb(200,158,80)",
    diffAdded: "rgb(230,255,236)",
    diffRemoved: "rgb(255,235,233)",
    diffAddedDimmed: "rgb(240,255,244)",
    diffRemovedDimmed: "rgb(255,245,244)",
    diffAddedWord: "rgb(172,242,189)",
    diffRemovedWord: "rgb(255,193,192)",
    red_FOR_SUBAGENTS_ONLY: "rgb(220,38,38)",
    blue_FOR_SUBAGENTS_ONLY: "rgb(37,99,235)",
    green_FOR_SUBAGENTS_ONLY: "rgb(22,163,74)",
    yellow_FOR_SUBAGENTS_ONLY: "rgb(202,138,4)",
    purple_FOR_SUBAGENTS_ONLY: "rgb(147,51,234)",
    orange_FOR_SUBAGENTS_ONLY: "rgb(234,88,12)",
    pink_FOR_SUBAGENTS_ONLY: "rgb(219,39,119)",
    cyan_FOR_SUBAGENTS_ONLY: "rgb(8,145,178)",
    professionalBlue: "rgb(106,155,204)",
    chromeYellow: "rgb(251,188,4)",
    rebonMascotBody: "rgb(70,117,164)",
    rebonMascotBackground: "rgb(0,0,0)",
    userMessageBackground: "rgb(240, 240, 240)",
    userMessageBackgroundHover: "rgb(252, 252, 252)",
    messageActionsBackground: "rgb(232, 236, 244)",
    selectionBg: "rgb(180, 213, 255)",
    bashMessageBackgroundColor: "rgb(250, 245, 250)",
    memoryBackgroundColor: "rgb(230, 245, 250)",
    rate_limit_fill: "rgb(87,105,247)",
    rate_limit_empty: "rgb(39,47,111)",
    fastMode: "rgb(255,106,0)",
    fastModeShimmer: "rgb(255,150,50)",
    briefLabelYou: "rgb(37,99,235)",
    briefLabelRebon: "rgb(70,117,164)",
    rainbow_red: "rgb(235,95,87)",
    rainbow_orange: "rgb(245,139,87)",
    rainbow_yellow: "rgb(250,195,95)",
    rainbow_green: "rgb(145,200,130)",
    rainbow_blue: "rgb(130,170,220)",
    rainbow_indigo: "rgb(155,130,200)",
    rainbow_violet: "rgb(200,130,180)",
    rainbow_red_shimmer: "rgb(250,155,147)",
    rainbow_orange_shimmer: "rgb(255,185,137)",
    rainbow_yellow_shimmer: "rgb(255,225,155)",
    rainbow_green_shimmer: "rgb(185,230,180)",
    rainbow_blue_shimmer: "rgb(180,205,240)",
    rainbow_indigo_shimmer: "rgb(195,180,230)",
    rainbow_violet_shimmer: "rgb(230,180,210)",
};

/// The `dark` palette: RGB literals tuned for a dark background.
#[allow(non_upper_case_globals)]
pub const DARK_THEME: Theme = Theme {
    autoAccept: "rgb(175,135,255)",
    bashBorder: "rgb(253,93,177)",
    rebon: "rgb(138,181,227)",
    rebonShimmer: "rgb(157,201,247)",
    rebonBlue_FOR_SYSTEM_SPINNER: "rgb(138,181,227)",
    rebonBlueShimmer_FOR_SYSTEM_SPINNER: "rgb(157,201,247)",
    permission: "rgb(177,185,249)",
    permissionShimmer: "rgb(207,215,255)",
    planMode: "rgb(72,150,140)",
    ide: "rgb(71,130,200)",
    promptBorder: "rgb(136,136,136)",
    promptBorderShimmer: "rgb(166,166,166)",
    text: "rgb(255,255,255)",
    inverseText: "rgb(0,0,0)",
    inactive: "rgb(153,153,153)",
    inactiveShimmer: "rgb(193,193,193)",
    subtle: "rgb(80,80,80)",
    suggestion: "rgb(177,185,249)",
    remember: "rgb(177,185,249)",
    background: "rgb(0,204,204)",
    success: "rgb(78,186,101)",
    error: "rgb(255,107,128)",
    warning: "rgb(255,193,7)",
    merged: "rgb(175,135,255)",
    warningShimmer: "rgb(255,223,57)",
    diffAdded: "rgb(18,38,30)",
    diffRemoved: "rgb(45,21,23)",
    diffAddedDimmed: "rgb(24,34,29)",
    diffRemovedDimmed: "rgb(38,25,27)",
    diffAddedWord: "rgb(40,95,59)",
    diffRemovedWord: "rgb(116,50,61)",
    red_FOR_SUBAGENTS_ONLY: "rgb(220,38,38)",
    blue_FOR_SUBAGENTS_ONLY: "rgb(37,99,235)",
    green_FOR_SUBAGENTS_ONLY: "rgb(22,163,74)",
    yellow_FOR_SUBAGENTS_ONLY: "rgb(202,138,4)",
    purple_FOR_SUBAGENTS_ONLY: "rgb(147,51,234)",
    orange_FOR_SUBAGENTS_ONLY: "rgb(234,88,12)",
    pink_FOR_SUBAGENTS_ONLY: "rgb(219,39,119)",
    cyan_FOR_SUBAGENTS_ONLY: "rgb(8,145,178)",
    professionalBlue: "rgb(106,155,204)",
    chromeYellow: "rgb(251,188,4)",
    rebonMascotBody: "rgb(138,181,227)",
    rebonMascotBackground: "rgb(0,0,0)",
    userMessageBackground: "rgb(55, 55, 55)",
    userMessageBackgroundHover: "rgb(70, 70, 70)",
    messageActionsBackground: "rgb(44, 50, 62)",
    selectionBg: "rgb(38, 79, 120)",
    bashMessageBackgroundColor: "rgb(65, 60, 65)",
    memoryBackgroundColor: "rgb(55, 65, 70)",
    rate_limit_fill: "rgb(177,185,249)",
    rate_limit_empty: "rgb(80,83,112)",
    fastMode: "rgb(255,120,20)",
    fastModeShimmer: "rgb(255,165,70)",
    briefLabelYou: "rgb(122,180,232)",
    briefLabelRebon: "rgb(138,181,227)",
    rainbow_red: "rgb(235,95,87)",
    rainbow_orange: "rgb(245,139,87)",
    rainbow_yellow: "rgb(250,195,95)",
    rainbow_green: "rgb(145,200,130)",
    rainbow_blue: "rgb(130,170,220)",
    rainbow_indigo: "rgb(155,130,200)",
    rainbow_violet: "rgb(200,130,180)",
    rainbow_red_shimmer: "rgb(250,155,147)",
    rainbow_orange_shimmer: "rgb(255,185,137)",
    rainbow_yellow_shimmer: "rgb(255,225,155)",
    rainbow_green_shimmer: "rgb(185,230,180)",
    rainbow_blue_shimmer: "rgb(180,205,240)",
    rainbow_indigo_shimmer: "rgb(195,180,230)",
    rainbow_violet_shimmer: "rgb(230,180,210)",
};

/// The `light-daltonized` palette: light mode with reds and greens pulled
/// apart for color-blind readers.
#[allow(non_upper_case_globals)]
pub const LIGHT_DALTONIZED_THEME: Theme = Theme {
    autoAccept: "rgb(135,0,255)",
    bashBorder: "rgb(0,102,204)",
    rebon: "rgb(70,117,164)",
    rebonShimmer: "rgb(47,99,151)",
    rebonBlue_FOR_SYSTEM_SPINNER: "rgb(70,117,164)",
    rebonBlueShimmer_FOR_SYSTEM_SPINNER: "rgb(47,99,151)",
    permission: "rgb(51,102,255)",
    permissionShimmer: "rgb(101,152,255)",
    planMode: "rgb(51,102,102)",
    ide: "rgb(71,130,200)",
    promptBorder: "rgb(153,153,153)",
    promptBorderShimmer: "rgb(183,183,183)",
    text: "rgb(0,0,0)",
    inverseText: "rgb(255,255,255)",
    inactive: "rgb(102,102,102)",
    inactiveShimmer: "rgb(142,142,142)",
    subtle: "rgb(175,175,175)",
    suggestion: "rgb(51,102,255)",
    remember: "rgb(51,102,255)",
    background: "rgb(0,153,153)",
    success: "rgb(0,102,153)",
    error: "rgb(204,0,0)",
    warning: "rgb(255,153,0)",
    merged: "rgb(135,0,255)",
    warningShimmer: "rgb(255,183,50)",
    diffAdded: "rgb(237,246,255)",
    diffRemoved: "rgb(255,235,233)",
    diffAddedDimmed: "rgb(245,250,255)",
    diffRemovedDimmed: "rgb(255,245,244)",
    diffAddedWord: "rgb(182,216,250)",
    diffRemovedWord: "rgb(255,193,192)",
    red_FOR_SUBAGENTS_ONLY: "rgb(204,0,0)",
    blue_FOR_SUBAGENTS_ONLY: "rgb(0,102,204)",
    green_FOR_SUBAGENTS_ONLY: "rgb(0,204,0)",
    yellow_FOR_SUBAGENTS_ONLY: "rgb(255,204,0)",
    purple_FOR_SUBAGENTS_ONLY: "rgb(128,0,128)",
    orange_FOR_SUBAGENTS_ONLY: "rgb(255,128,0)",
    pink_FOR_SUBAGENTS_ONLY: "rgb(255,102,178)",
    cyan_FOR_SUBAGENTS_ONLY: "rgb(0,178,178)",
    professionalBlue: "rgb(106,155,204)",
    chromeYellow: "rgb(251,188,4)",
    rebonMascotBody: "rgb(70,117,164)",
    rebonMascotBackground: "rgb(0,0,0)",
    userMessageBackground: "rgb(220, 220, 220)",
    userMessageBackgroundHover: "rgb(232, 232, 232)",
    messageActionsBackground: "rgb(210, 216, 226)",
    selectionBg: "rgb(180, 213, 255)",
    bashMessageBackgroundColor: "rgb(250, 245, 250)",
    memoryBackgroundColor: "rgb(230, 245, 250)",
    rate_limit_fill: "rgb(51,102,255)",
    rate_limit_empty: "rgb(23,46,114)",
    fastMode: "rgb(255,106,0)",
    fastModeShimmer: "rgb(255,150,50)",
    briefLabelYou: "rgb(37,99,235)",
    briefLabelRebon: "rgb(70,117,164)",
    rainbow_red: "rgb(235,95,87)",
    rainbow_orange: "rgb(245,139,87)",
    rainbow_yellow: "rgb(250,195,95)",
    rainbow_green: "rgb(145,200,130)",
    rainbow_blue: "rgb(130,170,220)",
    rainbow_indigo: "rgb(155,130,200)",
    rainbow_violet: "rgb(200,130,180)",
    rainbow_red_shimmer: "rgb(250,155,147)",
    rainbow_orange_shimmer: "rgb(255,185,137)",
    rainbow_yellow_shimmer: "rgb(255,225,155)",
    rainbow_green_shimmer: "rgb(185,230,180)",
    rainbow_blue_shimmer: "rgb(180,205,240)",
    rainbow_indigo_shimmer: "rgb(195,180,230)",
    rainbow_violet_shimmer: "rgb(230,180,210)",
};

/// The `dark-daltonized` palette: dark mode with reds and greens pulled
/// apart for color-blind readers.
#[allow(non_upper_case_globals)]
pub const DARK_DALTONIZED_THEME: Theme = Theme {
    autoAccept: "rgb(175,135,255)",
    bashBorder: "rgb(51,153,255)",
    rebon: "rgb(138,181,227)",
    rebonShimmer: "rgb(157,201,247)",
    rebonBlue_FOR_SYSTEM_SPINNER: "rgb(138,181,227)",
    rebonBlueShimmer_FOR_SYSTEM_SPINNER: "rgb(157,201,247)",
    permission: "rgb(153,204,255)",
    permissionShimmer: "rgb(183,224,255)",
    planMode: "rgb(102,153,153)",
    ide: "rgb(71,130,200)",
    promptBorder: "rgb(136,136,136)",
    promptBorderShimmer: "rgb(166,166,166)",
    text: "rgb(255,255,255)",
    inverseText: "rgb(0,0,0)",
    inactive: "rgb(153,153,153)",
    inactiveShimmer: "rgb(193,193,193)",
    subtle: "rgb(80,80,80)",
    suggestion: "rgb(153,204,255)",
    remember: "rgb(153,204,255)",
    background: "rgb(0,204,204)",
    success: "rgb(51,153,255)",
    error: "rgb(255,102,102)",
    warning: "rgb(255,204,0)",
    merged: "rgb(175,135,255)",
    warningShimmer: "rgb(255,234,50)",
    diffAdded: "rgb(18,42,56)",
    diffRemoved: "rgb(45,21,23)",
    diffAddedDimmed: "rgb(27,40,48)",
    diffRemovedDimmed: "rgb(38,25,27)",
    diffAddedWord: "rgb(36,95,130)",
    diffRemovedWord: "rgb(116,50,61)",
    red_FOR_SUBAGENTS_ONLY: "rgb(255,102,102)",
    blue_FOR_SUBAGENTS_ONLY: "rgb(102,178,255)",
    green_FOR_SUBAGENTS_ONLY: "rgb(102,255,102)",
    yellow_FOR_SUBAGENTS_ONLY: "rgb(255,255,102)",
    purple_FOR_SUBAGENTS_ONLY: "rgb(178,102,255)",
    orange_FOR_SUBAGENTS_ONLY: "rgb(255,178,102)",
    pink_FOR_SUBAGENTS_ONLY: "rgb(255,153,204)",
    cyan_FOR_SUBAGENTS_ONLY: "rgb(102,204,204)",
    professionalBlue: "rgb(106,155,204)",
    chromeYellow: "rgb(251,188,4)",
    rebonMascotBody: "rgb(138,181,227)",
    rebonMascotBackground: "rgb(0,0,0)",
    userMessageBackground: "rgb(55, 55, 55)",
    userMessageBackgroundHover: "rgb(70, 70, 70)",
    messageActionsBackground: "rgb(44, 50, 62)",
    selectionBg: "rgb(38, 79, 120)",
    bashMessageBackgroundColor: "rgb(65, 60, 65)",
    memoryBackgroundColor: "rgb(55, 65, 70)",
    rate_limit_fill: "rgb(153,204,255)",
    rate_limit_empty: "rgb(69,92,115)",
    fastMode: "rgb(255,120,20)",
    fastModeShimmer: "rgb(255,165,70)",
    briefLabelYou: "rgb(122,180,232)",
    briefLabelRebon: "rgb(138,181,227)",
    rainbow_red: "rgb(235,95,87)",
    rainbow_orange: "rgb(245,139,87)",
    rainbow_yellow: "rgb(250,195,95)",
    rainbow_green: "rgb(145,200,130)",
    rainbow_blue: "rgb(130,170,220)",
    rainbow_indigo: "rgb(155,130,200)",
    rainbow_violet: "rgb(200,130,180)",
    rainbow_red_shimmer: "rgb(250,155,147)",
    rainbow_orange_shimmer: "rgb(255,185,137)",
    rainbow_yellow_shimmer: "rgb(255,225,155)",
    rainbow_green_shimmer: "rgb(185,230,180)",
    rainbow_blue_shimmer: "rgb(180,205,240)",
    rainbow_indigo_shimmer: "rgb(195,180,230)",
    rainbow_violet_shimmer: "rgb(230,180,210)",
};

/// The `light-ansi` palette: named ANSI colors only, for terminals without
/// 24-bit color.
#[allow(non_upper_case_globals)]
pub const LIGHT_ANSI_THEME: Theme = Theme {
    autoAccept: "ansi:magenta",
    bashBorder: "ansi:magenta",
    rebon: "ansi:blue",
    rebonShimmer: "ansi:blueBright",
    rebonBlue_FOR_SYSTEM_SPINNER: "ansi:blue",
    rebonBlueShimmer_FOR_SYSTEM_SPINNER: "ansi:blueBright",
    permission: "ansi:blue",
    permissionShimmer: "ansi:blueBright",
    planMode: "ansi:cyan",
    ide: "ansi:blueBright",
    promptBorder: "ansi:white",
    promptBorderShimmer: "ansi:whiteBright",
    text: "ansi:black",
    inverseText: "ansi:white",
    inactive: "ansi:blackBright",
    inactiveShimmer: "ansi:white",
    subtle: "ansi:blackBright",
    suggestion: "ansi:blue",
    remember: "ansi:blue",
    background: "ansi:cyan",
    success: "ansi:green",
    error: "ansi:red",
    warning: "ansi:yellow",
    merged: "ansi:magenta",
    warningShimmer: "ansi:yellowBright",
    diffAdded: "ansi:green",
    diffRemoved: "ansi:red",
    diffAddedDimmed: "ansi:green",
    diffRemovedDimmed: "ansi:red",
    diffAddedWord: "ansi:greenBright",
    diffRemovedWord: "ansi:redBright",
    red_FOR_SUBAGENTS_ONLY: "ansi:red",
    blue_FOR_SUBAGENTS_ONLY: "ansi:blue",
    green_FOR_SUBAGENTS_ONLY: "ansi:green",
    yellow_FOR_SUBAGENTS_ONLY: "ansi:yellow",
    purple_FOR_SUBAGENTS_ONLY: "ansi:magenta",
    orange_FOR_SUBAGENTS_ONLY: "ansi:redBright",
    pink_FOR_SUBAGENTS_ONLY: "ansi:magentaBright",
    cyan_FOR_SUBAGENTS_ONLY: "ansi:cyan",
    professionalBlue: "ansi:blueBright",
    chromeYellow: "ansi:yellow",
    rebonMascotBody: "ansi:blue",
    rebonMascotBackground: "ansi:black",
    userMessageBackground: "ansi:white",
    userMessageBackgroundHover: "ansi:whiteBright",
    messageActionsBackground: "ansi:white",
    selectionBg: "ansi:cyan",
    bashMessageBackgroundColor: "ansi:whiteBright",
    memoryBackgroundColor: "ansi:white",
    rate_limit_fill: "ansi:yellow",
    rate_limit_empty: "ansi:black",
    fastMode: "ansi:red",
    fastModeShimmer: "ansi:redBright",
    briefLabelYou: "ansi:blue",
    briefLabelRebon: "ansi:blue",
    rainbow_red: "ansi:red",
    rainbow_orange: "ansi:redBright",
    rainbow_yellow: "ansi:yellow",
    rainbow_green: "ansi:green",
    rainbow_blue: "ansi:cyan",
    rainbow_indigo: "ansi:blue",
    rainbow_violet: "ansi:magenta",
    rainbow_red_shimmer: "ansi:redBright",
    rainbow_orange_shimmer: "ansi:yellow",
    rainbow_yellow_shimmer: "ansi:yellowBright",
    rainbow_green_shimmer: "ansi:greenBright",
    rainbow_blue_shimmer: "ansi:cyanBright",
    rainbow_indigo_shimmer: "ansi:blueBright",
    rainbow_violet_shimmer: "ansi:magentaBright",
};

/// The `dark-ansi` palette: named ANSI colors, with `professionalBlue` as
/// the single deliberate RGB exception.
#[allow(non_upper_case_globals)]
pub const DARK_ANSI_THEME: Theme = Theme {
    autoAccept: "ansi:magentaBright",
    bashBorder: "ansi:magentaBright",
    rebon: "ansi:blue",
    rebonShimmer: "ansi:blueBright",
    rebonBlue_FOR_SYSTEM_SPINNER: "ansi:blue",
    rebonBlueShimmer_FOR_SYSTEM_SPINNER: "ansi:blueBright",
    permission: "ansi:blueBright",
    permissionShimmer: "ansi:blueBright",
    planMode: "ansi:cyanBright",
    ide: "ansi:blue",
    promptBorder: "ansi:white",
    promptBorderShimmer: "ansi:whiteBright",
    text: "ansi:whiteBright",
    inverseText: "ansi:black",
    inactive: "ansi:white",
    inactiveShimmer: "ansi:whiteBright",
    subtle: "ansi:white",
    suggestion: "ansi:blueBright",
    remember: "ansi:blueBright",
    background: "ansi:cyanBright",
    success: "ansi:greenBright",
    error: "ansi:redBright",
    warning: "ansi:yellowBright",
    merged: "ansi:magentaBright",
    warningShimmer: "ansi:yellowBright",
    diffAdded: "ansi:green",
    diffRemoved: "ansi:red",
    diffAddedDimmed: "ansi:green",
    diffRemovedDimmed: "ansi:red",
    diffAddedWord: "ansi:greenBright",
    diffRemovedWord: "ansi:redBright",
    red_FOR_SUBAGENTS_ONLY: "ansi:redBright",
    blue_FOR_SUBAGENTS_ONLY: "ansi:blueBright",
    green_FOR_SUBAGENTS_ONLY: "ansi:greenBright",
    yellow_FOR_SUBAGENTS_ONLY: "ansi:yellowBright",
    purple_FOR_SUBAGENTS_ONLY: "ansi:magentaBright",
    orange_FOR_SUBAGENTS_ONLY: "ansi:redBright",
    pink_FOR_SUBAGENTS_ONLY: "ansi:magentaBright",
    cyan_FOR_SUBAGENTS_ONLY: "ansi:cyanBright",
    professionalBlue: "rgb(106,155,204)",
    chromeYellow: "ansi:yellowBright",
    rebonMascotBody: "ansi:blue",
    rebonMascotBackground: "ansi:black",
    userMessageBackground: "ansi:blackBright",
    userMessageBackgroundHover: "ansi:white",
    messageActionsBackground: "ansi:blackBright",
    selectionBg: "ansi:blue",
    bashMessageBackgroundColor: "ansi:black",
    memoryBackgroundColor: "ansi:blackBright",
    rate_limit_fill: "ansi:yellow",
    rate_limit_empty: "ansi:white",
    fastMode: "ansi:redBright",
    fastModeShimmer: "ansi:redBright",
    briefLabelYou: "ansi:blueBright",
    briefLabelRebon: "ansi:blue",
    rainbow_red: "ansi:red",
    rainbow_orange: "ansi:redBright",
    rainbow_yellow: "ansi:yellow",
    rainbow_green: "ansi:green",
    rainbow_blue: "ansi:cyan",
    rainbow_indigo: "ansi:blue",
    rainbow_violet: "ansi:magenta",
    rainbow_red_shimmer: "ansi:redBright",
    rainbow_orange_shimmer: "ansi:yellow",
    rainbow_yellow_shimmer: "ansi:yellowBright",
    rainbow_green_shimmer: "ansi:greenBright",
    rainbow_blue_shimmer: "ansi:cyanBright",
    rainbow_indigo_shimmer: "ansi:blueBright",
    rainbow_violet_shimmer: "ansi:magentaBright",
};

/// The palette for `name`.
///
/// There is deliberately no error case: `ThemeName` has exactly six
/// variants, so every input is covered. Code that wants a fallback asks
/// [`ThemeName::default`], which is `Dark`.
pub fn get_theme(name: ThemeName) -> Theme {
    match name {
        ThemeName::Light => LIGHT_THEME,
        ThemeName::LightAnsi => LIGHT_ANSI_THEME,
        ThemeName::DarkAnsi => DARK_ANSI_THEME,
        ThemeName::LightDaltonized => LIGHT_DALTONIZED_THEME,
        ThemeName::DarkDaltonized => DARK_DALTONIZED_THEME,
        ThemeName::Dark => DARK_THEME,
    }
}

/// The process-wide active theme, stored as an `AtomicUsize` holding the
/// `ThemeName` discriminant so loads and stores stay lock-free.
///
/// It exists for render paths that were not handed a theme — dialogs,
/// overlays, this crate's own primitives — which can call
/// [`get_active_theme`] instead of assuming `Dark`. The initial value is 0,
/// matching [`ThemeName::default`].
static ACTIVE_THEME: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn theme_name_to_idx(name: ThemeName) -> usize {
    match name {
        ThemeName::Dark => 0,
        ThemeName::Light => 1,
        ThemeName::LightDaltonized => 2,
        ThemeName::DarkDaltonized => 3,
        ThemeName::LightAnsi => 4,
        ThemeName::DarkAnsi => 5,
    }
}

fn idx_to_theme_name(idx: usize) -> ThemeName {
    match idx {
        1 => ThemeName::Light,
        2 => ThemeName::LightDaltonized,
        3 => ThemeName::DarkDaltonized,
        4 => ThemeName::LightAnsi,
        5 => ThemeName::DarkAnsi,
        _ => ThemeName::Dark,
    }
}

/// Replace the process-wide active theme. One relaxed atomic store, so it
/// is cheap enough to call from anywhere that changes the theme.
pub fn set_active_theme(name: ThemeName) {
    ACTIVE_THEME.store(
        theme_name_to_idx(name),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The current process-wide theme name.
pub fn active_theme_name() -> ThemeName {
    idx_to_theme_name(ACTIVE_THEME.load(std::sync::atomic::Ordering::Relaxed))
}

/// The current process-wide palette. Prefer this over a hardcoded palette
/// in render paths that were not handed a theme.
pub fn get_active_theme() -> Theme {
    get_theme(active_theme_name())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ────────────────────────────────────────────────────────────────
    // ThemeName parsing + naming
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn theme_name_default_is_dark() {
        assert_eq!(ThemeName::default(), ThemeName::Dark);
    }

    #[test]
    fn theme_name_round_trip_all_six() {
        for (name, kind) in THEME_NAMES {
            let parsed = ThemeName::from_str(name);
            assert_eq!(parsed, Some(*kind), "round-trip failed for {name}");
            assert_eq!(kind.as_str(), *name, "as_str mismatch for {name}");
        }
    }

    #[test]
    fn theme_name_unknown_returns_none() {
        assert!(ThemeName::from_str("Dark").is_none()); // case sensitive
        assert!(ThemeName::from_str("solarized").is_none());
        assert!(ThemeName::from_str("").is_none());
    }

    #[test]
    fn theme_settings_includes_auto_first() {
        // `auto` followed by every theme name.
        assert_eq!(THEME_SETTINGS[0], "auto");
        assert_eq!(THEME_SETTINGS.len(), 7);
    }

    // ────────────────────────────────────────────────────────────────
    // get_theme dispatch
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn get_theme_returns_the_canonical_rebon_accent() {
        // The slate rebrand splits the brand accent per mode: light themes use
        // the deep slate, dark themes the lifted slate.
        for (_, name) in THEME_NAMES {
            let expected = match name {
                ThemeName::LightAnsi | ThemeName::DarkAnsi => "ansi:blue",
                ThemeName::Light | ThemeName::LightDaltonized => "rgb(70,117,164)",
                ThemeName::Dark | ThemeName::DarkDaltonized => "rgb(138,181,227)",
            };
            assert_eq!(get_theme(*name).rebon, expected);
        }
    }

    #[test]
    fn dark_is_default_theme() {
        // Dark is the fallback theme.
        assert_eq!(get_theme(ThemeName::Dark), DARK_THEME);
    }

    // ────────────────────────────────────────────────────────────────
    // Pinned theme key values
    // ────────────────────────────────────────────────────────────────
    //
    // These tests pin a representative cross-section of every theme.
    // The full struct is matched 1:1; these tests are the canary
    // that the cross-section stays correct.

    #[test]
    fn light_theme_key_values() {
        let t = LIGHT_THEME;
        assert_eq!(t.autoAccept, "rgb(135,0,255)");
        assert_eq!(t.bashBorder, "rgb(255,0,135)");
        assert_eq!(t.rebon, "rgb(70,117,164)");
        assert_eq!(t.permission, "rgb(87,105,247)");
        assert_eq!(t.text, "rgb(0,0,0)");
        assert_eq!(t.inverseText, "rgb(255,255,255)");
        assert_eq!(t.success, "rgb(44,122,57)");
        assert_eq!(t.error, "rgb(171,43,63)");
        assert_eq!(t.warning, "rgb(150,108,30)");
        assert_eq!(t.professionalBlue, "rgb(106,155,204)");
        assert_eq!(t.userMessageBackground, "rgb(240, 240, 240)");
        assert_eq!(t.fastMode, "rgb(255,106,0)");
        assert_eq!(t.rainbow_red, "rgb(235,95,87)");
    }

    #[test]
    fn dark_theme_key_values() {
        let t = DARK_THEME;
        assert_eq!(t.autoAccept, "rgb(175,135,255)");
        assert_eq!(t.bashBorder, "rgb(253,93,177)");
        assert_eq!(t.rebon, "rgb(138,181,227)");
        assert_eq!(t.permission, "rgb(177,185,249)");
        assert_eq!(t.text, "rgb(255,255,255)");
        assert_eq!(t.inverseText, "rgb(0,0,0)");
        assert_eq!(t.success, "rgb(78,186,101)");
        assert_eq!(t.error, "rgb(255,107,128)");
        assert_eq!(t.warning, "rgb(255,193,7)");
        assert_eq!(t.professionalBlue, "rgb(106,155,204)");
        assert_eq!(t.userMessageBackground, "rgb(55, 55, 55)");
        assert_eq!(t.selectionBg, "rgb(38, 79, 120)");
    }

    #[test]
    fn light_ansi_theme_uses_only_ansi_colors() {
        // Every key starts with `ansi:`.
        let t = LIGHT_ANSI_THEME;
        let keys: Vec<&str> = vec![
            t.autoAccept,
            t.bashBorder,
            t.rebon,
            t.permission,
            t.text,
            t.inverseText,
            t.success,
            t.error,
            t.warning,
            t.professionalBlue,
            t.fastMode,
        ];
        for k in keys {
            assert!(k.starts_with("ansi:"), "expected ansi: prefix, got `{k}`");
        }
    }

    #[test]
    fn dark_ansi_professional_blue_is_rgb_not_ansi() {
        // DARK_ANSI_THEME.professionalBlue is the
        // ONLY non-ansi color in the dark-ansi palette. This is a
        // load-bearing exception, not a typo.
        assert_eq!(DARK_ANSI_THEME.professionalBlue, "rgb(106,155,204)");
    }

    #[test]
    fn dark_ansi_theme_keys() {
        let t = DARK_ANSI_THEME;
        assert_eq!(t.text, "ansi:whiteBright");
        assert_eq!(t.inverseText, "ansi:black");
        assert_eq!(t.success, "ansi:greenBright");
        assert_eq!(t.error, "ansi:redBright");
        assert_eq!(t.bashBorder, "ansi:magentaBright");
    }

    #[test]
    fn light_daltonized_theme_keys() {
        let t = LIGHT_DALTONIZED_THEME;
        assert_eq!(t.success, "rgb(0,102,153)"); // blue not green
        assert_eq!(t.bashBorder, "rgb(0,102,204)"); // blue not pink
        assert_eq!(t.rebon, "rgb(70,117,164)"); // light-mode slate accent
    }

    #[test]
    fn dark_daltonized_theme_keys() {
        let t = DARK_DALTONIZED_THEME;
        assert_eq!(t.success, "rgb(51,153,255)"); // blue
        assert_eq!(t.error, "rgb(255,102,102)");
        assert_eq!(t.warning, "rgb(255,204,0)");
        assert_eq!(t.bashBorder, "rgb(51,153,255)"); // blue
    }

    // ────────────────────────────────────────────────────────────────
    // Theme.lookup
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn lookup_known_rebon_keys() {
        assert_eq!(DARK_THEME.lookup("rebon"), Some("rgb(138,181,227)"));
        assert_eq!(
            DARK_THEME.lookup("briefLabelRebon"),
            Some("rgb(138,181,227)")
        );
        assert_eq!(DARK_THEME.lookup("error"), Some("rgb(255,107,128)"));
        assert_eq!(LIGHT_THEME.lookup("error"), Some("rgb(171,43,63)"));
    }

    #[test]
    fn default_diff_palettes_use_subtle_row_tints() {
        assert_eq!(
            (
                LIGHT_THEME.diffAdded,
                LIGHT_THEME.diffRemoved,
                LIGHT_THEME.diffAddedWord,
                LIGHT_THEME.diffRemovedWord,
            ),
            (
                "rgb(230,255,236)",
                "rgb(255,235,233)",
                "rgb(172,242,189)",
                "rgb(255,193,192)",
            )
        );
        assert_eq!(
            (
                DARK_THEME.diffAdded,
                DARK_THEME.diffRemoved,
                DARK_THEME.diffAddedWord,
                DARK_THEME.diffRemovedWord,
            ),
            (
                "rgb(18,38,30)",
                "rgb(45,21,23)",
                "rgb(40,95,59)",
                "rgb(116,50,61)",
            )
        );
    }

    #[test]
    fn lookup_unknown_key_returns_none() {
        assert_eq!(DARK_THEME.lookup("not_a_key"), None);
        assert_eq!(DARK_THEME.lookup(""), None);
        assert_eq!(DARK_THEME.lookup("REBON"), None); // case sensitive
    }

    #[test]
    fn lookup_every_field_is_dispatchable() {
        // Spot-check that the `lookup()` switch and the field list
        // stay in sync. If a future refactor adds a field to the
        // struct but forgets to add it to `lookup()`, this test
        // catches it for the most common keys.
        let t = DARK_THEME;
        let must_resolve = [
            "autoAccept",
            "bashBorder",
            "rebon",
            "rebonShimmer",
            "rebonBlue_FOR_SYSTEM_SPINNER",
            "rebonBlueShimmer_FOR_SYSTEM_SPINNER",
            "permission",
            "permissionShimmer",
            "planMode",
            "ide",
            "promptBorder",
            "promptBorderShimmer",
            "text",
            "inverseText",
            "inactive",
            "inactiveShimmer",
            "subtle",
            "suggestion",
            "remember",
            "background",
            "success",
            "error",
            "warning",
            "merged",
            "warningShimmer",
            "diffAdded",
            "diffRemoved",
            "diffAddedDimmed",
            "diffRemovedDimmed",
            "diffAddedWord",
            "diffRemovedWord",
            "red_FOR_SUBAGENTS_ONLY",
            "blue_FOR_SUBAGENTS_ONLY",
            "green_FOR_SUBAGENTS_ONLY",
            "yellow_FOR_SUBAGENTS_ONLY",
            "purple_FOR_SUBAGENTS_ONLY",
            "orange_FOR_SUBAGENTS_ONLY",
            "pink_FOR_SUBAGENTS_ONLY",
            "cyan_FOR_SUBAGENTS_ONLY",
            "professionalBlue",
            "chromeYellow",
            "rebonMascotBody",
            "rebonMascotBackground",
            "userMessageBackground",
            "userMessageBackgroundHover",
            "messageActionsBackground",
            "selectionBg",
            "bashMessageBackgroundColor",
            "memoryBackgroundColor",
            "rate_limit_fill",
            "rate_limit_empty",
            "fastMode",
            "fastModeShimmer",
            "briefLabelYou",
            "briefLabelRebon",
            "rainbow_red",
            "rainbow_orange",
            "rainbow_yellow",
            "rainbow_green",
            "rainbow_blue",
            "rainbow_indigo",
            "rainbow_violet",
            "rainbow_red_shimmer",
            "rainbow_orange_shimmer",
            "rainbow_yellow_shimmer",
            "rainbow_green_shimmer",
            "rainbow_blue_shimmer",
            "rainbow_indigo_shimmer",
            "rainbow_violet_shimmer",
        ];
        assert_eq!(must_resolve.len(), 69); // 69 fields
        for k in must_resolve {
            assert!(
                t.lookup(k).is_some(),
                "field `{k}` is not dispatchable via lookup()"
            );
        }
    }
}
