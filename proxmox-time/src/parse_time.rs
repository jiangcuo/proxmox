use std::collections::HashMap;

use anyhow::{bail, Error};
use lazy_static::lazy_static;

use super::time::*;
use super::daily_duration::*;

use nom::{
    error::{context, ParseError, VerboseError},
    bytes::complete::{tag, take_while1},
    combinator::{map_res, all_consuming, opt, recognize},
    sequence::{pair, preceded, terminated, tuple},
    character::complete::{alpha1, space0, digit1},
    multi::separated_nonempty_list,
};

use crate::parse_helpers::{parse_complete_line, parse_error, parse_hm_time, parse_time_comp, parse_u64, IResult};
use crate::{parse_weekdays_range, WeekDays};
use crate::date_time_value::DateTimeValue;

lazy_static! {
    static ref TIME_SPAN_UNITS: HashMap<&'static str, f64> = {
        let mut map = HashMap::new();

        let second = 1.0;

        map.insert("seconds", second);
        map.insert("second", second);
        map.insert("sec", second);
        map.insert("s", second);

        let msec = second / 1000.0;

        map.insert("msec", msec);
        map.insert("ms", msec);

        let usec = msec / 1000.0;

        map.insert("usec", usec);
        map.insert("us", usec);
        map.insert("µs", usec);

        let nsec = usec / 1000.0;

        map.insert("nsec", nsec);
        map.insert("ns", nsec);

        let minute = second * 60.0;

        map.insert("minutes", minute);
        map.insert("minute", minute);
        map.insert("min", minute);
        map.insert("m", minute);

        let hour = minute * 60.0;

        map.insert("hours", hour);
        map.insert("hour", hour);
        map.insert("hr", hour);
        map.insert("h", hour);

        let day = hour * 24.0 ;

        map.insert("days", day);
        map.insert("day", day);
        map.insert("d", day);

        let week = day * 7.0;

        map.insert("weeks", week);
        map.insert("week", week);
        map.insert("w", week);

        let month = 30.44 * day;

        map.insert("months", month);
        map.insert("month", month);
        map.insert("M", month);

        let year = 365.25 * day;

        map.insert("years", year);
        map.insert("year", year);
        map.insert("y", year);

        map
    };
}

struct TimeSpec {
    hour: Vec<DateTimeValue>,
    minute: Vec<DateTimeValue>,
    second: Vec<DateTimeValue>,
}

struct DateSpec {
    year: Vec<DateTimeValue>,
    month: Vec<DateTimeValue>,
    day: Vec<DateTimeValue>,
}

fn parse_date_time_comp(max: usize) -> impl Fn(&str) -> IResult<&str, DateTimeValue> {
    move |i: &str| {
        let (i, value) = parse_time_comp(max)(i)?;

        if let (i, Some(end)) = opt(preceded(tag(".."), parse_time_comp(max)))(i)? {
            if value > end {
                return Err(parse_error(i, "range start is bigger than end"));
            }
            if let Some(time) = i.strip_prefix('/') {
                let (time, repeat) = parse_time_comp(max)(time)?;
                return Ok((time, DateTimeValue::Repeated(value, repeat, Some(end))));
            }
            return Ok((i, DateTimeValue::Range(value, end)));
        }

        if let Some(time) = i.strip_prefix('/') {
            let (time, repeat) = parse_time_comp(max)(time)?;
            Ok((time, DateTimeValue::Repeated(value, repeat, None)))
        } else {
            Ok((i, DateTimeValue::Single(value)))
        }
    }
}

fn parse_date_time_comp_list(start: u32, max: usize) -> impl Fn(&str) -> IResult<&str, Vec<DateTimeValue>> {
    move |i: &str| {
        if let Some(rest) = i.strip_prefix('*') {
            if let Some(time) = rest.strip_prefix('/') {
                let (n, repeat) = parse_time_comp(max)(time)?;
                if repeat > 0 {
                    return Ok((n, vec![DateTimeValue::Repeated(start, repeat, None)]));
                }
            }
            return Ok((rest, Vec::new()));
        }

        separated_nonempty_list(tag(","), parse_date_time_comp(max))(i)
    }
}

fn parse_time_spec(i: &str) -> IResult<&str, TimeSpec> {

    let (i, (opt_hour, minute, opt_second)) = tuple((
        opt(terminated(parse_date_time_comp_list(0, 24), tag(":"))),
        parse_date_time_comp_list(0, 60),
        opt(preceded(tag(":"), parse_date_time_comp_list(0, 60))),
    ))(i)?;

    let hour = opt_hour.unwrap_or_else(Vec::new);
    let second = opt_second.unwrap_or_else(|| vec![DateTimeValue::Single(0)]);

    Ok((i, TimeSpec { hour, minute, second }))
}

fn parse_date_spec(i: &str) -> IResult<&str, DateSpec> {

    // TODO: implement ~ for days (man systemd.time)
    if let Ok((i, (year, month, day))) = tuple((
        parse_date_time_comp_list(0, 2200), // the upper limit for systemd, stay compatible
        preceded(tag("-"), parse_date_time_comp_list(1, 13)),
        preceded(tag("-"), parse_date_time_comp_list(1, 32)),
    ))(i) {
        Ok((i, DateSpec { year, month, day }))
    } else if let Ok((i, (month, day))) = tuple((
        parse_date_time_comp_list(1, 13),
        preceded(tag("-"), parse_date_time_comp_list(1, 32)),
    ))(i) {
        Ok((i, DateSpec { year: Vec::new(), month, day }))
    } else {
        Err(parse_error(i, "invalid date spec"))
    }
}

/// Parse a [CalendarEvent]
pub fn parse_calendar_event(i: &str) -> Result<CalendarEvent, Error> {
    parse_complete_line("calendar event", i, parse_calendar_event_incomplete)
}

fn parse_calendar_event_incomplete(mut i: &str) -> IResult<&str, CalendarEvent> {

    let mut has_dayspec = false;
    let mut has_timespec = false;
    let mut has_datespec = false;

    let mut event = CalendarEvent::default();

    if i.starts_with(|c: char| char::is_ascii_alphabetic(&c)) {

        match i {
            "minutely" => {
                return Ok(("", CalendarEvent {
                    second: vec![DateTimeValue::Single(0)],
                    ..Default::default()
                }));
            }
            "hourly" => {
                return Ok(("", CalendarEvent {
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    ..Default::default()
                }));
            }
            "daily" => {
                return Ok(("", CalendarEvent {
                    hour: vec![DateTimeValue::Single(0)],
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    ..Default::default()
                }));
            }
            "weekly" => {
                return Ok(("", CalendarEvent {
                    hour: vec![DateTimeValue::Single(0)],
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    days: WeekDays::MONDAY,
                    ..Default::default()
                }));
            }
            "monthly" => {
                return Ok(("", CalendarEvent {
                    hour: vec![DateTimeValue::Single(0)],
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    day: vec![DateTimeValue::Single(1)],
                    ..Default::default()
                }));
            }
            "yearly" | "annually" => {
                return Ok(("", CalendarEvent {
                    hour: vec![DateTimeValue::Single(0)],
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    day: vec![DateTimeValue::Single(1)],
                    month: vec![DateTimeValue::Single(1)],
                    ..Default::default()
                }));
            }
            "quarterly" => {
                return Ok(("", CalendarEvent {
                    hour: vec![DateTimeValue::Single(0)],
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    day: vec![DateTimeValue::Single(1)],
                    month: vec![
                        DateTimeValue::Single(1),
                        DateTimeValue::Single(4),
                        DateTimeValue::Single(7),
                        DateTimeValue::Single(10),
                    ],
                    ..Default::default()
                }));
            }
            "semiannually" | "semi-annually" => {
                return Ok(("", CalendarEvent {
                    hour: vec![DateTimeValue::Single(0)],
                    minute: vec![DateTimeValue::Single(0)],
                    second: vec![DateTimeValue::Single(0)],
                    day: vec![DateTimeValue::Single(1)],
                    month: vec![
                        DateTimeValue::Single(1),
                        DateTimeValue::Single(7),
                    ],
                    ..Default::default()
                }));
            }
            _ => { /* continue */ }
        }

        let (n, range_list) =  context(
            "weekday range list",
            separated_nonempty_list(tag(","), parse_weekdays_range)
        )(i)?;

        has_dayspec = true;

        i = space0(n)?.0;

        for range in range_list  { event.days.insert(range); }
    }

    if let (n, Some(date)) = opt(parse_date_spec)(i)? {
        event.year = date.year;
        event.month = date.month;
        event.day = date.day;
        has_datespec = true;
        i = space0(n)?.0;
    }

    if let (n, Some(time)) = opt(parse_time_spec)(i)? {
        event.hour = time.hour;
        event.minute = time.minute;
        event.second = time.second;
        has_timespec = true;
        i = n;
    } else {
        event.hour = vec![DateTimeValue::Single(0)];
        event.minute = vec![DateTimeValue::Single(0)];
        event.second = vec![DateTimeValue::Single(0)];
    }

    if !(has_dayspec || has_timespec || has_datespec) {
        return Err(parse_error(i, "date or time specification"));
    }

    Ok((i, event))
}

fn parse_time_unit(i: &str) ->  IResult<&str, &str> {
    let (n, text) = take_while1(|c: char| char::is_ascii_alphabetic(&c) || c == 'µ')(i)?;
    if TIME_SPAN_UNITS.contains_key(&text) {
        Ok((n, text))
    } else {
        Err(parse_error(text, "time unit"))
    }
}


/// Parse a [TimeSpan]
pub fn parse_time_span(i: &str) -> Result<TimeSpan, Error> {
    parse_complete_line("time span", i, parse_time_span_incomplete)
}

fn parse_time_span_incomplete(mut i: &str) -> IResult<&str, TimeSpan> {

    let mut ts = TimeSpan::default();

    loop {
        i = space0(i)?.0;
        if i.is_empty() { break; }
        let (n, num) = parse_u64(i)?;
        i = space0(n)?.0;

        if let (n, Some(unit)) = opt(parse_time_unit)(i)? {
            i = n;
            match unit {
                "seconds" | "second" | "sec" | "s" => {
                    ts.seconds += num;
                }
                "msec" | "ms" => {
                    ts.msec += num;
                }
                "usec" | "us" | "µs" => {
                    ts.usec += num;
                }
                "nsec" | "ns" => {
                    ts.nsec += num;
                }
                "minutes" | "minute" | "min" | "m" => {
                    ts.minutes += num;
                }
                "hours" | "hour" | "hr" | "h" => {
                    ts.hours += num;
                }
                "days" | "day" | "d" => {
                    ts.days += num;
                }
                "weeks" | "week" | "w" => {
                    ts.weeks += num;
                }
                "months" | "month" | "M" => {
                    ts.months += num;
                }
                "years" | "year" | "y" => {
                    ts.years += num;
                }
                _ => return Err(parse_error(unit, "internal error")),
            }
        } else {
            ts.seconds += num;
        }
    }

    Ok((i, ts))
}
