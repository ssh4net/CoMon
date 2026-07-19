use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayStyle {
    Classic,
    SystemCompact,
    SystemFull,
}

impl DisplayStyle {
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Classic => Self::SystemCompact,
            Self::SystemCompact => Self::SystemFull,
            Self::SystemFull => Self::Classic,
        }
    }

    pub(crate) fn store_value(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::SystemCompact => "system-compact",
            Self::SystemFull => "system-full",
        }
    }

    pub(crate) fn from_store(value: &str) -> Option<Self> {
        match value {
            "classic" => Some(Self::Classic),
            "system" | "system-compact" => Some(Self::SystemCompact),
            "system-full" => Some(Self::SystemFull),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateOrder {
    DayMonthYear,
    MonthDayYear,
    YearMonthDay,
}

#[derive(Debug, Clone)]
pub(crate) struct SystemLocale {
    id: String,
    decimal_separator: String,
    thousands_separator: String,
    date_order: DateOrder,
    date_separator: String,
    time_24h: bool,
    time_separator: String,
    abbreviated_weekdays: [String; 7],
    abbreviated_months: [String; 12],
    am: String,
    pm: String,
}

impl SystemLocale {
    pub(crate) fn detect() -> Self {
        platform::detect().unwrap_or_else(Self::c_locale)
    }

    fn c_locale() -> Self {
        Self {
            id: "C".to_string(),
            decimal_separator: ".".to_string(),
            thousands_separator: String::new(),
            date_order: DateOrder::MonthDayYear,
            date_separator: "/".to_string(),
            time_24h: true,
            time_separator: ":".to_string(),
            abbreviated_weekdays: english_weekdays(),
            abbreviated_months: english_months(),
            am: "AM".to_string(),
            pm: "PM".to_string(),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Default for SystemLocale {
    fn default() -> Self {
        Self::c_locale()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DisplayFormatter<'a> {
    style: DisplayStyle,
    system: &'a SystemLocale,
}

impl<'a> DisplayFormatter<'a> {
    pub(crate) fn new(style: DisplayStyle, system: &'a SystemLocale) -> Self {
        Self { style, system }
    }

    pub(crate) fn style(self) -> DisplayStyle {
        self.style
    }

    pub(crate) fn style_label(self) -> String {
        match self.style {
            DisplayStyle::Classic => "Classic".to_string(),
            DisplayStyle::SystemCompact => {
                format!("System Compact ({})", self.system.id())
            }
            DisplayStyle::SystemFull => format!("System Full ({})", self.system.id()),
        }
    }

    pub(crate) fn format_count(self, value: i64) -> String {
        self.format_u64(value.max(0) as u64)
    }

    pub(crate) fn format_usize(self, value: usize) -> String {
        self.format_u64(value as u64)
    }

    pub(crate) fn format_u64(self, value: u64) -> String {
        let separator = match self.style {
            DisplayStyle::Classic => ",",
            DisplayStyle::SystemCompact | DisplayStyle::SystemFull => {
                self.system.thousands_separator.as_str()
            }
        };
        group_decimal_digits(value, separator)
    }

    pub(crate) fn localize_decimal(self, value: &str) -> String {
        if self.style == DisplayStyle::Classic || self.system.decimal_separator == "." {
            return value.to_string();
        }
        value.replacen('.', &self.system.decimal_separator, 1)
    }

    pub(crate) fn format_one_decimal(self, value: f64) -> String {
        self.localize_decimal(&format!("{:.1}", value))
    }

    pub(crate) fn format_chart_day(self, date: NaiveDate) -> String {
        if self.style == DisplayStyle::Classic {
            return format!(
                "{} {:02}/{:02}",
                classic_weekday(date.weekday()),
                date.month(),
                date.day()
            );
        }
        let weekday = &self.system.abbreviated_weekdays[weekday_index(date.weekday())];
        let compact = self.system.format_numeric_date(date, false);
        format!("{weekday} {compact}")
    }

    pub(crate) fn format_short_date(self, date: NaiveDate) -> String {
        if self.style == DisplayStyle::Classic {
            return format!("{} {:02}", english_month(date.month()), date.day());
        }
        let month = &self.system.abbreviated_months[date.month0() as usize];
        match self.system.date_order {
            DateOrder::MonthDayYear => format!("{month} {:02}", date.day()),
            DateOrder::DayMonthYear | DateOrder::YearMonthDay => {
                format!("{:02} {month}", date.day())
            }
        }
    }

    pub(crate) fn abbreviated_month(self, month: u32) -> String {
        if self.style == DisplayStyle::Classic {
            return english_month(month).to_string();
        }
        self.system
            .abbreviated_months
            .get(month.saturating_sub(1) as usize)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn abbreviated_weekday(self, day: Weekday) -> String {
        if self.style == DisplayStyle::Classic {
            return classic_weekday(day).to_string();
        }
        self.system.abbreviated_weekdays[weekday_index(day)].clone()
    }

    pub(crate) fn format_full_date(self, date: NaiveDate) -> String {
        if self.style == DisplayStyle::Classic {
            return date.format("%Y-%m-%d").to_string();
        }
        self.system.format_numeric_date(date, true)
    }

    pub(crate) fn format_time(self, date_time: NaiveDateTime) -> String {
        if self.style == DisplayStyle::Classic {
            return format!("{:02}:{:02}", date_time.hour(), date_time.minute());
        }
        let separator = &self.system.time_separator;
        if self.system.time_24h {
            return format!(
                "{:02}{separator}{:02}",
                date_time.hour(),
                date_time.minute()
            );
        }
        let hour = match date_time.hour() % 12 {
            0 => 12,
            value => value,
        };
        let marker = if date_time.hour() < 12 {
            &self.system.am
        } else {
            &self.system.pm
        };
        format!("{hour}{separator}{:02} {marker}", date_time.minute())
    }

    pub(crate) fn format_session_datetime(self, date_time: NaiveDateTime) -> String {
        format!(
            "{} {}",
            self.format_full_date(date_time.date()),
            self.format_time(date_time)
        )
    }

    pub(crate) fn format_reset_datetime(self, date_time: NaiveDateTime) -> String {
        format!(
            "{}, {}",
            self.format_reset_date(date_time.date()),
            self.format_time(date_time)
        )
    }

    pub(crate) fn format_reset_date(self, date: NaiveDate) -> String {
        if self.style == DisplayStyle::Classic {
            return format!("{} {}", date.day(), english_month(date.month()));
        }
        self.format_short_date(date)
    }
}

impl SystemLocale {
    fn format_numeric_date(&self, date: NaiveDate, include_year: bool) -> String {
        let separator = &self.date_separator;
        let day = format!("{:02}", date.day());
        let month = format!("{:02}", date.month());
        let year = format!("{:04}", date.year());
        let pieces: Vec<&str> = match (self.date_order, include_year) {
            (DateOrder::DayMonthYear, true) => vec![&day, &month, &year],
            (DateOrder::DayMonthYear, false) => vec![&day, &month],
            (DateOrder::MonthDayYear, true) => vec![&month, &day, &year],
            (DateOrder::MonthDayYear, false) => vec![&month, &day],
            (DateOrder::YearMonthDay, true) => vec![&year, &month, &day],
            (DateOrder::YearMonthDay, false) => vec![&month, &day],
        };
        pieces.join(separator)
    }
}

fn group_decimal_digits(value: u64, separator: &str) -> String {
    let digits = value.to_string();
    if separator.is_empty() || digits.len() <= 3 {
        return digits;
    }
    let first = match digits.len() % 3 {
        0 => 3,
        value => value,
    };
    let mut out = String::with_capacity(digits.len() + separator.len() * (digits.len() / 3));
    out.push_str(&digits[..first]);
    let mut offset = first;
    while offset < digits.len() {
        out.push_str(separator);
        out.push_str(&digits[offset..offset + 3]);
        offset += 3;
    }
    out
}

fn weekday_index(day: Weekday) -> usize {
    day.num_days_from_monday() as usize
}

fn classic_weekday(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

fn english_month(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "",
    }
}

fn english_weekdays() -> [String; 7] {
    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map(str::to_string)
}

fn english_months() -> [String; 12] {
    [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .map(str::to_string)
}

fn parse_posix_date_pattern(pattern: &str) -> (DateOrder, String) {
    let mut fields = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            continue;
        }
        let Some(code) = chars.next() else { break };
        let field = match code {
            'd' | 'e' => Some('d'),
            'm' | 'b' | 'B' | 'h' => Some('m'),
            'y' | 'Y' => Some('y'),
            _ => None,
        };
        if let Some(field) = field {
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
    }
    let order = date_order_from_fields(&fields);
    let separator = pattern
        .chars()
        .find(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '%' | ' ' | '-' | '_'))
        .or_else(|| pattern.chars().find(|ch| matches!(ch, '/' | '.' | '-')))
        .unwrap_or('-')
        .to_string();
    (order, separator)
}

#[cfg(any(windows, test))]
fn parse_windows_date_pattern(pattern: &str) -> (DateOrder, String) {
    let mut fields = Vec::new();
    let mut quoted = false;
    for ch in pattern.chars() {
        if ch == '\'' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        let field = match ch {
            'd' => Some('d'),
            'M' => Some('m'),
            'y' => Some('y'),
            _ => None,
        };
        if let Some(field) = field {
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
    }
    let order = date_order_from_fields(&fields);
    let separator = pattern
        .chars()
        .find(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '\'' | ' '))
        .unwrap_or('-')
        .to_string();
    (order, separator)
}

fn parse_posix_time_separator(pattern: &str) -> String {
    for hour_code in ["%H", "%I", "%k", "%l"] {
        if let Some(offset) = pattern.find(hour_code) {
            let tail = &pattern[offset + hour_code.len()..];
            if let Some(separator) = tail
                .chars()
                .find(|ch| !ch.is_ascii_alphanumeric() && *ch != '%')
            {
                return separator.to_string();
            }
        }
    }
    ":".to_string()
}

#[cfg(any(windows, test))]
fn parse_windows_time_separator(pattern: &str) -> String {
    let mut saw_hour = false;
    for ch in pattern.chars() {
        if !saw_hour {
            saw_hour = matches!(ch, 'h' | 'H');
            continue;
        }
        if matches!(ch, 'h' | 'H') {
            continue;
        }
        if ch == 'm' {
            break;
        }
        if !ch.is_ascii_alphanumeric() && ch != '\'' {
            return ch.to_string();
        }
    }
    ":".to_string()
}

fn date_order_from_fields(fields: &[char]) -> DateOrder {
    match fields.first().copied() {
        Some('d') => DateOrder::DayMonthYear,
        Some('y') => DateOrder::YearMonthDay,
        _ => DateOrder::MonthDayYear,
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::{CStr, CString};

    pub(super) fn detect() -> Option<SystemLocale> {
        let requested = CString::new("").ok()?;
        let locale =
            unsafe { libc::newlocale(libc::LC_ALL_MASK, requested.as_ptr(), std::ptr::null_mut()) };
        let locale = if locale.is_null() {
            let fallback = CString::new("C").ok()?;
            unsafe { libc::newlocale(libc::LC_ALL_MASK, fallback.as_ptr(), std::ptr::null_mut()) }
        } else {
            locale
        };
        if locale.is_null() {
            return None;
        }

        let previous = unsafe { libc::uselocale(locale) };
        let profile = capture_current_thread_locale();
        unsafe {
            libc::uselocale(previous);
            libc::freelocale(locale);
        }
        profile
    }

    fn capture_current_thread_locale() -> Option<SystemLocale> {
        let decimal_separator = langinfo(libc::RADIXCHAR)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ".".to_string());
        let thousands_separator = langinfo(libc::THOUSEP).unwrap_or_default();
        let date_pattern = langinfo(libc::D_FMT).unwrap_or_else(|| "%m/%d/%y".to_string());
        let time_pattern = langinfo(libc::T_FMT).unwrap_or_else(|| "%H:%M:%S".to_string());
        let (date_order, date_separator) = parse_posix_date_pattern(&date_pattern);
        let abbreviated_weekdays = [
            langinfo(libc::ABDAY_2),
            langinfo(libc::ABDAY_3),
            langinfo(libc::ABDAY_4),
            langinfo(libc::ABDAY_5),
            langinfo(libc::ABDAY_6),
            langinfo(libc::ABDAY_7),
            langinfo(libc::ABDAY_1),
        ]
        .map(|value| value.unwrap_or_default());
        let abbreviated_months = [
            langinfo(libc::ABMON_1),
            langinfo(libc::ABMON_2),
            langinfo(libc::ABMON_3),
            langinfo(libc::ABMON_4),
            langinfo(libc::ABMON_5),
            langinfo(libc::ABMON_6),
            langinfo(libc::ABMON_7),
            langinfo(libc::ABMON_8),
            langinfo(libc::ABMON_9),
            langinfo(libc::ABMON_10),
            langinfo(libc::ABMON_11),
            langinfo(libc::ABMON_12),
        ]
        .map(|value| value.unwrap_or_default());
        let id = locale_id_from_env();
        Some(SystemLocale {
            id,
            decimal_separator,
            thousands_separator,
            date_order,
            date_separator,
            time_24h: time_pattern.contains("%H") || time_pattern.contains("%k"),
            time_separator: parse_posix_time_separator(&time_pattern),
            abbreviated_weekdays: fill_empty(abbreviated_weekdays, english_weekdays()),
            abbreviated_months: fill_empty(abbreviated_months, english_months()),
            am: langinfo(libc::AM_STR)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "AM".to_string()),
            pm: langinfo(libc::PM_STR)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "PM".to_string()),
        })
    }

    fn langinfo(item: libc::nl_item) -> Option<String> {
        let ptr = unsafe { libc::nl_langinfo(item) };
        if ptr.is_null() {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn locale_id_from_env() -> String {
        for key in ["LC_ALL", "LC_TIME", "LC_NUMERIC", "LANG"] {
            if let Ok(value) = std::env::var(key) {
                let value = value.trim();
                if !value.is_empty() {
                    return value.to_string();
                }
            }
        }
        "C".to_string()
    }

    fn fill_empty<const N: usize>(mut values: [String; N], fallback: [String; N]) -> [String; N] {
        for (value, fallback) in values.iter_mut().zip(fallback) {
            if value.is_empty() {
                *value = fallback;
            }
        }
        values
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::Globalization::*;

    pub(super) fn detect() -> Option<SystemLocale> {
        let id = user_locale_name().unwrap_or_else(|| "system".to_string());
        let date_pattern = locale_string(LOCALE_SSHORTDATE)?;
        let time_pattern = locale_string(LOCALE_STIMEFORMAT)?;
        let (date_order, date_separator) = parse_windows_date_pattern(&date_pattern);
        let abbreviated_weekdays = std::array::from_fn(|index| {
            locale_string(LOCALE_SABBREVDAYNAME1 + index as u32).unwrap_or_default()
        });
        let abbreviated_months = std::array::from_fn(|index| {
            locale_string(LOCALE_SABBREVMONTHNAME1 + index as u32).unwrap_or_default()
        });
        Some(SystemLocale {
            id,
            decimal_separator: locale_string(LOCALE_SDECIMAL).unwrap_or_else(|| ".".to_string()),
            thousands_separator: locale_string(LOCALE_STHOUSAND).unwrap_or_default(),
            date_order,
            date_separator,
            time_24h: time_pattern.contains('H'),
            time_separator: parse_windows_time_separator(&time_pattern),
            abbreviated_weekdays: fill_empty(abbreviated_weekdays, english_weekdays()),
            abbreviated_months: fill_empty(abbreviated_months, english_months()),
            am: locale_string(LOCALE_S1159).unwrap_or_else(|| "AM".to_string()),
            pm: locale_string(LOCALE_S2359).unwrap_or_else(|| "PM".to_string()),
        })
    }

    fn locale_string(kind: u32) -> Option<String> {
        let required = unsafe { GetLocaleInfoEx(std::ptr::null(), kind, std::ptr::null_mut(), 0) };
        if required <= 1 {
            return None;
        }
        let mut buffer = vec![0u16; required as usize];
        let written =
            unsafe { GetLocaleInfoEx(std::ptr::null(), kind, buffer.as_mut_ptr(), required) };
        if written <= 1 {
            return None;
        }
        buffer.truncate((written - 1) as usize);
        String::from_utf16(&buffer).ok()
    }

    fn user_locale_name() -> Option<String> {
        let mut buffer = [0u16; 85];
        let written = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
        if written <= 1 {
            return None;
        }
        String::from_utf16(&buffer[..(written - 1) as usize]).ok()
    }

    fn fill_empty<const N: usize>(mut values: [String; N], fallback: [String; N]) -> [String; N] {
        for (value, fallback) in values.iter_mut().zip(fallback) {
            if value.is_empty() {
                *value = fallback;
            }
        }
        values
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    pub(super) fn detect() -> Option<SystemLocale> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn european_profile() -> SystemLocale {
        SystemLocale {
            id: "et-EE".to_string(),
            decimal_separator: ",".to_string(),
            thousands_separator: "\u{a0}".to_string(),
            date_order: DateOrder::DayMonthYear,
            date_separator: ".".to_string(),
            time_24h: true,
            time_separator: ":".to_string(),
            abbreviated_weekdays: english_weekdays(),
            abbreviated_months: english_months(),
            am: "AM".to_string(),
            pm: "PM".to_string(),
        }
    }

    #[test]
    fn classic_preserves_existing_number_format() {
        let system = european_profile();
        let formatter = DisplayFormatter::new(DisplayStyle::Classic, &system);
        assert_eq!(formatter.format_count(1_234_567), "1,234,567");
        assert_eq!(formatter.localize_decimal("1.23M"), "1.23M");
    }

    #[test]
    fn display_style_cycles_and_accepts_legacy_system_value() {
        assert_eq!(DisplayStyle::Classic.toggled(), DisplayStyle::SystemCompact);
        assert_eq!(
            DisplayStyle::SystemCompact.toggled(),
            DisplayStyle::SystemFull
        );
        assert_eq!(DisplayStyle::SystemFull.toggled(), DisplayStyle::Classic);
        assert_eq!(
            DisplayStyle::from_store("system"),
            Some(DisplayStyle::SystemCompact)
        );
    }

    #[test]
    fn system_uses_detected_number_separators() {
        let system = european_profile();
        let formatter = DisplayFormatter::new(DisplayStyle::SystemCompact, &system);
        assert_eq!(formatter.format_count(1_234_567), "1\u{a0}234\u{a0}567");
        assert_eq!(formatter.localize_decimal("1.23M"), "1,23M");
    }

    #[test]
    fn system_formats_dates_and_time_from_profile() {
        let system = european_profile();
        let formatter = DisplayFormatter::new(DisplayStyle::SystemCompact, &system);
        let date = NaiveDate::from_ymd_opt(2026, 3, 4).unwrap();
        let date_time = date.and_hms_opt(14, 5, 0).unwrap();
        assert_eq!(formatter.format_chart_day(date), "Wed 04.03");
        assert_eq!(formatter.format_short_date(date), "04 Mar");
        assert_eq!(
            formatter.format_session_datetime(date_time),
            "04.03.2026 14:05"
        );
    }

    #[test]
    fn parses_common_posix_and_windows_date_patterns() {
        assert_eq!(
            parse_posix_date_pattern("%m/%d/%y"),
            (DateOrder::MonthDayYear, "/".to_string())
        );
        assert_eq!(
            parse_posix_date_pattern("%d.%m.%Y"),
            (DateOrder::DayMonthYear, ".".to_string())
        );
        assert_eq!(
            parse_windows_date_pattern("yyyy-MM-dd"),
            (DateOrder::YearMonthDay, "-".to_string())
        );
        assert_eq!(
            parse_windows_date_pattern("dd.MM.yyyy"),
            (DateOrder::DayMonthYear, ".".to_string())
        );
        assert_eq!(parse_posix_time_separator("%H.%M.%S"), ".");
        assert_eq!(parse_windows_time_separator("HH.mm.ss"), ".");
    }
}
