// Generates src/stdlib/intl_data.rs, the date formatting data `Date.prototype.toLocale*` uses (2.4.1, #64938), from
// the Intl.DateTimeFormat of the Bun that runs it (opencode 2.4.1 runs Code Mode on Bun 1.4.4, whose ICU carries
// CLDR's data, under the Unicode license). Run with Bun 1.4.4:
//
//   bun run intl-data.ts > crates/oc-codemode/src/stdlib/intl_data.rs
//
// For each locale: the month, weekday, era and day-period names, the time zone names of a few zones, the offset
// format, and the pattern for each set of options in OPTION_SETS, read from formatToParts at reference times and
// written as a pattern of literal text and `{field}` tokens.
const LOCALES = [
  "en-US",
  "en-GB",
  "en-CA",
  "en-AU",
  "en-IN",
  "de-DE",
  "fr-FR",
  "fr-CA",
  "es-ES",
  "es-MX",
  "it-IT",
  "pt-BR",
  "nl-NL",
  "sv-SE",
  "pl-PL",
  "ja-JP",
  "zh-CN",
  "zh-TW",
  "ko-KR",
  "ru-RU",
]
const ZONES = [
  "UTC",
  "America/New_York",
  "America/Chicago",
  "America/Denver",
  "America/Los_Angeles",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Asia/Tokyo",
  "Asia/Shanghai",
  "Asia/Kolkata",
  "Australia/Sydney",
  "Europe/Moscow",
  "Europe/Madrid",
  "Europe/Rome",
  "America/Sao_Paulo",
  "America/Mexico_City",
  "America/Toronto",
  "Asia/Seoul",
  "Asia/Singapore",
  "Pacific/Auckland",
  "Africa/Cairo",
]
const NAME_TYPES = ["short", "long", "shortGeneric", "longGeneric"] as const

// Reference wall times in UTC: Tuesday 9 January 2024 (a month whose names differ in every width), at 00:04:07.089,
// 09:04:07.089 and 13:04:07.089.
const at = (h: number) => new Date(Date.UTC(2024, 0, 9, h, 4, 7, 89))
const WINTER = new Date(Date.UTC(2024, 0, 15, 12))
const SUMMER = new Date(Date.UTC(2024, 6, 15, 12))
/** January and July as [standard time, daylight time]: the later offset is daylight (south of the equator, January). */
const offsetMinutes = (zone: string, d: Date) => {
  const parts = new Intl.DateTimeFormat("en-US", { timeZone: zone, hourCycle: "h23", year: "numeric", month: "numeric", day: "numeric", hour: "numeric", minute: "numeric" }).formatToParts(d)
  const n = (t: string) => Number(parts.find((p) => p.type === t)!.value)
  return (Date.UTC(n("year"), n("month") - 1, n("day"), n("hour"), n("minute")) - d.getTime()) / 60000
}
const seasons = (zone: string) => (offsetMinutes(zone, WINTER) > offsetMinutes(zone, SUMMER) ? [SUMMER, WINTER] : [WINTER, SUMMER])

type Options = Record<string, string | number | boolean>
const KEYS: Array<[string, Array<string | number | boolean>]> = [
  ["weekday", ["narrow", "short", "long"]],
  ["era", ["narrow", "short", "long"]],
  ["year", ["numeric", "2-digit"]],
  ["month", ["numeric", "2-digit", "narrow", "short", "long"]],
  ["day", ["numeric", "2-digit"]],
  ["hour", ["numeric", "2-digit"]],
  ["minute", ["numeric", "2-digit"]],
  ["second", ["numeric", "2-digit"]],
  ["fractionalSecondDigits", [1, 2, 3]],
  ["timeZoneName", ["short", "long", "shortOffset", "longOffset", "shortGeneric", "longGeneric"]],
  ["hc", ["t", "f", "h11", "h12", "h23", "h24"]],
  ["dayPeriod", ["narrow", "short", "long"]],
  ["dateStyle", ["full", "long", "medium", "short"]],
  ["timeStyle", ["full", "long", "medium", "short"]],
]

/** The option set as a key: one code per option in KEYS order, `-` for absent. Mirrored by `date_format.rs`. */
export const keyOf = (o: Options) =>
  KEYS.map(([name, values]) => {
    const v = o[name]
    return v === undefined ? "-" : String.fromCharCode(97 + values.indexOf(v))
  }).join("")

const toIntl = (o: Options) => {
  const out: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(o)) {
    if (k === "hc") {
      if (v === "t") out.hour12 = true
      else if (v === "f") out.hour12 = false
      else out.hourCycle = v
    } else out[k] = v
  }
  return out
}

const OPTION_SETS: Array<Options> = []
const add = (o: Options) => OPTION_SETS.push(o)
for (const ds of [undefined, "full", "long", "medium", "short"])
  for (const ts of [undefined, "full", "long", "medium", "short"]) {
    if (ds === undefined && ts === undefined) continue
    const o: Options = {}
    if (ds) o.dateStyle = ds
    if (ts) o.timeStyle = ts
    add(o)
    if (ts) for (const hc of ["t", "f"]) add({ ...o, hc })
  }
const dateSets: Array<Options> = []
for (const weekday of [undefined, "narrow", "short", "long"])
  for (const year of [undefined, "numeric", "2-digit"])
    for (const month of [undefined, "numeric", "2-digit", "narrow", "short", "long"])
      for (const day of [undefined, "numeric", "2-digit"]) {
        const o: Options = {}
        if (weekday) o.weekday = weekday
        if (year) o.year = year
        if (month) o.month = month
        if (day) o.day = day
        if (Object.keys(o).length) dateSets.push(o)
      }
const timeSets: Array<Options> = []
for (const hour of [undefined, "numeric", "2-digit"])
  for (const minute of [undefined, "numeric", "2-digit"])
    for (const second of [undefined, "numeric", "2-digit"]) {
      const o: Options = {}
      if (hour) o.hour = hour
      if (minute) o.minute = minute
      if (second) o.second = second
      if (!Object.keys(o).length) continue
      for (const hc of [undefined, "t", "f"]) timeSets.push(hc ? { ...o, hc } : o)
      if (o.hour) for (const hc of ["h11", "h12", "h23", "h24"]) add({ ...o, hc })
    }
const DATE: Options = { year: "numeric", month: "numeric", day: "numeric" }
const TIME: Options = { hour: "numeric", minute: "numeric", second: "numeric" }
for (const o of dateSets) add(o)
for (const o of timeSets) add(o)
for (const t of timeSets) for (const d of [DATE, { weekday: "long", ...DATE }, { year: "numeric", month: "short", day: "numeric" }, { year: "numeric", month: "long", day: "numeric" }, { weekday: "long", year: "numeric", month: "long", day: "numeric" }, { month: "short", day: "numeric" }, { weekday: "short", month: "short", day: "numeric" }]) add({ ...d, ...t })
for (const hc of ["h11", "h12", "h23", "h24"]) add({ ...DATE, ...TIME, hc })
for (const weekday of ["narrow", "short", "long"]) for (const hc of [undefined, "t", "f"]) add(hc ? { weekday, ...TIME, hc } : { weekday, ...TIME })
for (const tz of ["short", "long", "shortOffset", "longOffset", "shortGeneric", "longGeneric"])
  for (const base of [TIME, { ...DATE, ...TIME }, DATE, { hour: "numeric", minute: "2-digit" }, { hour: "2-digit", minute: "2-digit" }, { hour: "numeric" }, { hour: "2-digit" }])
    for (const hc of [undefined, "t", "f"]) add(hc ? { ...base, timeZoneName: tz, hc } : { ...base, timeZoneName: tz })
for (const era of ["narrow", "short", "long"]) {
  add({ ...DATE, era })
  add({ year: "numeric", era })
  add({ ...DATE, ...TIME, era })
  for (const hc of [undefined, "t", "f"]) add(hc ? { era, ...TIME, hc } : { era, ...TIME })
}
for (const t of timeSets) if (t.hour) for (const weekday of ["narrow", "short", "long"]) add({ weekday, ...t })
for (const d of [{ month: "long" }, { month: "short" }, { month: "long", day: "numeric" }, { month: "short", day: "numeric" }, { month: "numeric", day: "numeric" }, { year: "numeric", month: "long" }])
  for (const hc of [undefined, "t", "f"]) add(hc ? { ...d, ...TIME, hc } : { ...d, ...TIME })
for (const t of timeSets) if (t.second) for (const f of [1, 2, 3]) add({ ...t, fractionalSecondDigits: f })
for (const f of [1, 2, 3]) {
  add({ second: "numeric", fractionalSecondDigits: f })
  add({ second: "2-digit", fractionalSecondDigits: f })
  add({ ...DATE, second: "numeric", fractionalSecondDigits: f })
}
for (const dayPeriod of ["narrow", "short", "long"]) {
  add({ dayPeriod })
  for (const t of [{ hour: "numeric" }, { hour: "numeric", minute: "2-digit" }, TIME, { hour: "2-digit", minute: "2-digit" }])
    for (const hc of [undefined, "t"]) add(hc ? { ...t, dayPeriod, hc } : { ...t, dayPeriod })
}
for (const f of [1, 2, 3]) {
  add({ ...TIME, fractionalSecondDigits: f })
  add({ ...DATE, ...TIME, fractionalSecondDigits: f })
  add({ minute: "2-digit", second: "2-digit", fractionalSecondDigits: f })
}

const names = (locale: string, options: Options, dates: Array<Date>, type: string) =>
  dates.map((d) => new Intl.DateTimeFormat(locale, { timeZone: "UTC", ...options }).formatToParts(d).find((p) => p.type === type)?.value ?? "")
const months = Array.from({ length: 12 }, (_, m) => new Date(Date.UTC(2024, m, 5, 12)))
const days = Array.from({ length: 7 }, (_, d) => new Date(Date.UTC(2024, 2, 3 + d, 12)))

const rust = (s: string) => JSON.stringify(s)
const UNICODE_LICENSE = `// UNICODE LICENSE V3
//
// COPYRIGHT AND PERMISSION NOTICE
//
// Copyright © 1991-2024 Unicode, Inc.
//
// NOTICE TO USER: Carefully read the following legal agreement. BY
// DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
// SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
// TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
// DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of data files and any associated documentation (the "Data Files") or
// software and any associated documentation (the "Software") to deal in the
// Data Files or Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, and/or sell
// copies of the Data Files or Software, and to permit persons to whom the
// Data Files or Software are furnished to do so, provided that either (a)
// this copyright and permission notice appear with all copies of the Data
// Files or Software, or (b) this copyright and permission notice appear in
// associated Documentation.
//
// THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
// KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
// THIRD PARTY RIGHTS.
//
// IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
// BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
// OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
// WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
// ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
// FILES OR SOFTWARE.
//
// Except as contained in this notice, the name of a copyright holder shall
// not be used in advertising or otherwise to promote the sale, use or other
// dealings in these Data Files or Software without prior written
// authorization of the copyright holder.
//
// SPDX-License-Identifier: Unicode-3.0`
const out: Array<string> = []
out.push("// @generated by tests/recorder/intl-data.ts from Bun 1.4.4's Intl (ICU with CLDR data). Do not edit; regenerate.")
out.push("//")
out.push("// The names and patterns are Unicode CLDR data, used under the Unicode License v3:")
out.push("//")
out.push(UNICODE_LICENSE)
out.push("#![allow(clippy::all)]")
out.push("pub struct Locale {")
out.push("    pub tag: &'static str,")
out.push("    /// Months and weekdays: [format, standalone] x [narrow, short, long].")
out.push("    pub months: [[[&'static str; 12]; 3]; 2],")
out.push("    pub weekdays: [[[&'static str; 7]; 3]; 2],")
out.push("    /// Eras BC and AD: [narrow, short, long].")
out.push("    pub eras: [[&'static str; 2]; 3],")
out.push("    pub am_pm: [&'static str; 2],")
out.push("    /// Flexible day periods by hour (at :04) and at 12:00 exactly: [narrow, short, long].")
out.push("    pub day_periods: [[&'static str; 25]; 3],")
out.push("    /// Zone names: (zone, [short, long, shortGeneric, longGeneric] x [standard, daylight]).")
out.push("    pub zones: &'static [(&'static str, [[&'static str; 2]; 4])],")
out.push("    /// The offset formats: zero, positive and negative examples of `shortOffset` and `longOffset`.")
out.push("    pub gmt_zero: &'static str,")
out.push("    pub offsets: [&'static str; 4],")
out.push("    /// The pattern of each key of `KEYS`, as an index into `PATTERNS` (`u16::MAX`: none).")
out.push("    pub patterns: &'static [u16],")
out.push("}")
out.push("")
const perLocale: Array<Map<string, string>> = []
const unique = new Map<string, number>()
const intern = (p: string) => {
  if (!unique.has(p)) unique.set(p, unique.size)
  return unique.get(p)!
}
out.push("pub static LOCALES: &[Locale] = &[")
for (const locale of LOCALES) {
  const ampm = names(locale, { hour: "numeric", hourCycle: "h12" }, [at(9), at(13)], "dayPeriod")
  const pattern = (options: Options): string | undefined => {
    const intl = toIntl(options)
    let fmt: Intl.DateTimeFormat
    try {
      fmt = new Intl.DateTimeFormat(locale, { timeZone: options.timeZoneName ? "America/New_York" : "UTC", ...intl })
    } catch {
      return undefined
    }
    const zone = options.timeZoneName ? "America/New_York" : "UTC"
    const shift = (d: Date) => (zone === "UTC" ? d : new Date(d.getTime() + 5 * 3600 * 1000))
    const parts = [0, 9, 13].map((h) => fmt.formatToParts(shift(at(h))))
    const text = parts[2]!.map((p) => {
      if (p.type === "literal") return p.value.replace(/[{}]/g, (c) => (c === "{" ? "{{" : "}}"))
      const v = p.value
      switch (p.type) {
        case "year":
          return v.length === 2 ? "{yy}" : "{y}"
        case "month": {
          if (/^\d+$/.test(v)) return v.length === 2 ? "{MM}" : "{M}"
          // Which width, and the format (f) or standalone (s) form, from the month names this pattern gives over
          // the whole year (one month's forms can coincide).
          const year = months.map((d) => fmt.formatToParts(new Date(d.getTime() + 5 * 3600 * 1000 * Number(zone !== "UTC"))).find((p) => p.type === "month")!.value)
          for (const w of ["long", "short", "narrow"] as const) {
            if (names(locale, { month: w, day: "numeric" }, months, "month").join("|") === year.join("|")) return `{M:${w}:f}`
            if (names(locale, { month: w }, months, "month").join("|") === year.join("|")) return `{M:${w}:s}`
          }
          return "{M:long:f}"
        }
        case "day":
          return v.length === 2 ? "{dd}" : "{d}"
        case "weekday": {
          for (const w of ["long", "short", "narrow"] as const) {
            if (names(locale, { weekday: w, day: "numeric" }, [at(13)], "weekday")[0] === v) return `{E:${w}:f}`
            if (names(locale, { weekday: w }, [at(13)], "weekday")[0] === v) return `{E:${w}:s}`
          }
          return "{E:long:f}"
        }
        case "era": {
          const width = (["narrow", "short", "long"] as const).find((w) => names(locale, { era: w, year: "numeric" }, [at(13)], "era")[0] === v)
          return `{G:${width ?? "short"}}`
        }
        case "hour": {
          const at0 = parts[0]!.find((p) => p.type === "hour")!.value
          const at9 = parts[1]!.find((p) => p.type === "hour")!.value
          const cycle = v === "13" ? (at0 === "24" ? "k" : "H") : at0 === "12" ? "h" : "K"
          return `{${at9.length === 2 ? cycle + cycle : cycle}}`
        }
        case "minute":
          return v.length === 2 ? "{mm}" : "{m}"
        case "second":
          return v.length === 2 ? "{ss}" : "{s}"
        case "fractionalSecond":
          return `{${"S".repeat(v.length)}}`
        case "dayPeriod": {
          // Flexible day periods (`in the morning`, `清晨`): asked for, or a locale's default (zh-TW).
          if (options.dayPeriod) return `{B:${options.dayPeriod}}`
          const at0 = parts[0]!.find((p) => p.type === "dayPeriod")?.value
          return at0 !== undefined && at0 !== ampm[0] ? "{B:short}" : "{a}"
        }
        case "timeZoneName":
          // A time style names the zone too: long for `full`, short for `long`.
          return `{z:${options.timeZoneName ?? (options.timeStyle === "full" ? "long" : "short")}}`
        default:
          return `{?${p.type}}`
      }
    })
    return text.join("")
  }
  const nameSet = (key: "month" | "weekday", dates: Array<Date>) =>
    [false, true].map((alone) =>
      (["narrow", "short", "long"] as const).map((w) => names(locale, alone ? { [key]: w } : { [key]: w, day: "numeric" }, dates, key)),
    )
  const mon = nameSet("month", months)
  const wd = nameSet("weekday", days)
  const eraDates = [new Date(Date.UTC(-100, 0, 1)), new Date(Date.UTC(2024, 0, 1))]
  const eras = (["narrow", "short", "long"] as const).map((w) => names(locale, { era: w, year: "numeric" }, eraDates, "era"))
  const hours = Array.from({ length: 24 }, (_, h) => new Date(Date.UTC(2024, 0, 9, h, 4)))
  const flexible = (["narrow", "short", "long"] as const).map((w) => [
    ...names(locale, { hour: "numeric", hourCycle: "h12", dayPeriod: w }, hours, "dayPeriod"),
    names(locale, { hour: "numeric", hourCycle: "h12", dayPeriod: w }, [new Date(Date.UTC(2024, 0, 9, 12))], "dayPeriod")[0]!,
  ])
  const zoneNames = ZONES.map((zone) => {
    const byType = NAME_TYPES.map((type) =>
      seasons(zone).map(
        (d) => new Intl.DateTimeFormat(locale, { timeZone: zone, timeZoneName: type }).formatToParts(d).find((p) => p.type === "timeZoneName")!.value,
      ),
    )
    return `(${rust(zone)}, [${byType.map((pair) => `[${pair.map(rust).join(", ")}]`).join(", ")}])`
  })
  const offset = (zone: string, type: string) =>
    new Intl.DateTimeFormat(locale, { timeZone: zone, timeZoneName: type }).formatToParts(WINTER).find((p) => p.type === "timeZoneName")!.value
  const patterns = new Map<string, string>()
  for (const o of OPTION_SETS) {
    const p = pattern(o)
    if (p !== undefined) patterns.set(keyOf(o), p)
  }
  perLocale.push(patterns)
  const arr = (list: Array<string>) => `[${list.map(rust).join(", ")}]`
  out.push("    Locale {")
  out.push(`        tag: ${rust(locale)},`)
  out.push(`        months: [${mon.map((ctx) => `[${ctx.map(arr).join(", ")}]`).join(", ")}],`)
  out.push(`        weekdays: [${wd.map((ctx) => `[${ctx.map(arr).join(", ")}]`).join(", ")}],`)
  out.push(`        eras: [${eras.map(arr).join(", ")}],`)
  out.push(`        am_pm: ${arr(ampm)},`)
  out.push(`        day_periods: [${flexible.map(arr).join(", ")}],`)
  out.push(`        zones: &[${zoneNames.join(", ")}],`)
  out.push(`        gmt_zero: ${rust(offset("UTC", "shortOffset"))},`)
  out.push(`        offsets: ${arr([offset("Asia/Kolkata", "shortOffset"), offset("America/New_York", "shortOffset"), offset("Asia/Kolkata", "longOffset"), offset("America/New_York", "longOffset")])},`)
  out.push(`        patterns: &PATTERNS_${LOCALES.indexOf(locale)},`)
  out.push("    },")
}
out.push("];")
out.push("")
const allKeys = [...new Set(perLocale.flatMap((m) => [...m.keys()]))].sort()
out.push(`/// Every option key with a pattern, sorted.`)
out.push(`pub static KEYS: &[&str] = &[${allKeys.map(rust).join(", ")}];`)
perLocale.forEach((m, i) => {
  out.push(`static PATTERNS_${i}: [u16; ${allKeys.length}] = [${allKeys.map((k) => (m.has(k) ? intern(m.get(k)!) : 65535)).join(", ")}];`)
})
out.push(`pub static PATTERNS: &[&str] = &[${[...unique.keys()].map(rust).join(", ")}];`)
// English names of every zone, for the English locales' zones outside their own list.
const english = Intl.supportedValuesOf("timeZone").map((zone) => {
  const byType = NAME_TYPES.map((type) =>
    seasons(zone).map(
      (d) => new Intl.DateTimeFormat("en-US", { timeZone: zone, timeZoneName: type }).formatToParts(d).find((p) => p.type === "timeZoneName")!.value,
    ),
  )
  return `(${rust(zone)}, [${byType.map((pair) => `[${pair.map(rust).join(", ")}]`).join(", ")}])`
})
out.push(`/// en-US's zone names for every zone, as \`Locale::zones\`.`)
out.push(`pub static EN_ZONES: &[(&str, [[&str; 2]; 4])] = &[${english.join(", ")}];`)
console.log(out.join("\n"))
