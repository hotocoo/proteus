//! Where the guest believes it is.
//!
//! The Android image's location service asks one of its terminals for a position about once a
//! second by writing a command to it, and reads back one line describing a fix. Until a position
//! has been set here nothing is answered, and the image reports its built-in default.
//!
//! The position can also move: along a route read from a GPX file, or walking in a direction. A
//! moving position is worked out from the time each time the guest asks, so nothing runs between
//! requests.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::copy;

use devices::virtio::console::ConsoleHandle;

/// Terminal the image asks for its position on (`/dev/hvc7`).
pub const PORT: usize = 7;

const REQUEST: &[u8] = b"CMD_GET_LOCATION";
/// What the image reads as the end of an answer.
const END_OF_ANSWER: &str = "\n\n\n\n";
const PROVIDER: &str = "GPS";
/// How exact the guest is told the position is.
const ACCURACY_METRES: f64 = 3.0;
const MAX_LATITUDE: f64 = 90.0;
const MAX_LONGITUDE: f64 = 180.0;
/// The Earth's mean radius, for distances along its surface.
const EARTH_RADIUS_METRES: f64 = 6_371_000.0;
const SECONDS_PER_HOUR: f64 = 3600.0;
const METRES_PER_KM: f64 = 1000.0;
const FULL_TURN_DEGREES: f64 = 360.0;
/// GPX point elements, for routes, tracks and lists of waypoints.
const GPX_POINTS: [&str; 3] = ["<trkpt", "<rtept", "<wpt"];

/// A position: degrees north, degrees east, metres above sea level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: f64,
}

impl Fix {
    /// `None` unless the numbers are a place on Earth.
    pub fn new(latitude: f64, longitude: f64, altitude: f64) -> Option<Self> {
        let valid = latitude.abs() <= MAX_LATITUDE
            && longitude.abs() <= MAX_LONGITUDE
            && altitude.is_finite();
        valid.then_some(Self {
            latitude,
            longitude,
            altitude,
        })
    }
}

/// Where a moving position is: the place, metres per second and degrees clockwise from north.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Moving {
    fix: Fix,
    speed: f64,
    bearing: f64,
}

/// Distance in metres and initial bearing in degrees from one place to another, over the surface.
fn distance_and_bearing(from: &Fix, to: &Fix) -> (f64, f64) {
    let (lat1, lat2) = (from.latitude.to_radians(), to.latitude.to_radians());
    let d_lat = lat2 - lat1;
    let d_lon = (to.longitude - from.longitude).to_radians();
    let a = (d_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
    let distance = 2.0 * EARTH_RADIUS_METRES * a.sqrt().asin();
    let y = d_lon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * d_lon.cos();
    (
        distance,
        y.atan2(x).to_degrees().rem_euclid(FULL_TURN_DEGREES),
    )
}

/// The place `metres` from `from` heading `bearing` degrees.
fn travel(from: &Fix, bearing: f64, metres: f64) -> Fix {
    let angle = metres / EARTH_RADIUS_METRES;
    let (lat1, lon1, heading) = (
        from.latitude.to_radians(),
        from.longitude.to_radians(),
        bearing.to_radians(),
    );
    let lat2 = (lat1.sin() * angle.cos() + lat1.cos() * angle.sin() * heading.cos()).asin();
    let lon2 = lon1
        + (heading.sin() * angle.sin() * lat1.cos()).atan2(angle.cos() - lat1.sin() * lat2.sin());
    let longitude =
        (lon2.to_degrees() + MAX_LONGITUDE).rem_euclid(FULL_TURN_DEGREES) - MAX_LONGITUDE;
    Fix {
        latitude: lat2.to_degrees(),
        longitude,
        altitude: from.altitude,
    }
}

/// How the position is changing.
#[derive(Clone, Debug, PartialEq)]
enum Plan {
    Still(Fix),
    /// Walking from a place in one direction, since a moment.
    Walk {
        from: Fix,
        bearing: f64,
        speed: f64,
        since: Instant,
    },
    /// Following the points of a route, at a speed, since a moment. It stops at the last point.
    Route {
        points: Vec<Fix>,
        speed: f64,
        since: Instant,
    },
}

impl Plan {
    fn at(&self, now: Instant) -> Moving {
        match self {
            Self::Still(fix) => Moving {
                fix: *fix,
                speed: 0.0,
                bearing: 0.0,
            },
            Self::Walk {
                from,
                bearing,
                speed,
                since,
            } => {
                let metres = speed * now.saturating_duration_since(*since).as_secs_f64();
                Moving {
                    fix: travel(from, *bearing, metres),
                    speed: *speed,
                    bearing: *bearing,
                }
            }
            Self::Route {
                points,
                speed,
                since,
            } => {
                let mut left = speed * now.saturating_duration_since(*since).as_secs_f64();
                for pair in points.windows(2) {
                    let (length, bearing) = distance_and_bearing(&pair[0], &pair[1]);
                    if left < length {
                        let mut fix = travel(&pair[0], bearing, left);
                        let share = left / length;
                        fix.altitude =
                            pair[0].altitude + (pair[1].altitude - pair[0].altitude) * share;
                        return Moving {
                            fix,
                            speed: *speed,
                            bearing,
                        };
                    }
                    left -= length;
                }
                // `read_gpx` never gives an empty route.
                let last = points.last().copied().unwrap_or(Fix {
                    latitude: 0.0,
                    longitude: 0.0,
                    altitude: 0.0,
                });
                Moving {
                    fix: last,
                    speed: 0.0,
                    bearing: 0.0,
                }
            }
        }
    }
}

/// Reads the points of a GPX file: its track points, route points or waypoints, in order.
pub fn read_gpx(text: &str) -> Result<Vec<Fix>, String> {
    // A file can hold a track and waypoints both; the track is the route, so the kinds of point
    // are tried in order and the first one present is used.
    for element in GPX_POINTS {
        let points = gpx_points(text, element)?;
        if !points.is_empty() {
            return Ok(points);
        }
    }
    Err(copy::GPX_EMPTY.to_owned())
}

/// The value of attribute `name` in the text of a tag, if it is a number in quotes.
fn gpx_attribute(tag: &str, name: &str) -> Option<f64> {
    let at = tag.find(&format!(" {name}="))? + name.len() + 2;
    let quote = tag[at..]
        .chars()
        .next()
        .filter(|quote| *quote == '"' || *quote == '\'')?;
    let rest = &tag[at + quote.len_utf8()..];
    rest[..rest.find(quote)?].trim().parse().ok()
}

/// Every point written as `element` in a GPX file, in order.
fn gpx_points(text: &str, element: &str) -> Result<Vec<Fix>, String> {
    let mut points = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(element) {
        let after = &rest[at + element.len()..];
        let tag_end = after.find('>').ok_or(copy::GPX_BROKEN)?;
        let tag = &after[..tag_end];
        // Elevation, if the point has one inside it.
        let body = if tag.ends_with('/') {
            ""
        } else {
            let inside = &after[tag_end + 1..];
            let close = format!("</{}", &element[1..]);
            &inside[..inside.find(&close).unwrap_or(inside.len())]
        };
        let elevation = body.split_once("<ele>").and_then(|(_, ele)| {
            ele.split_once("</ele>")
                .and_then(|(value, _)| value.trim().parse().ok())
        });
        let latitude = gpx_attribute(tag, "lat").ok_or(copy::GPX_BROKEN)?;
        let longitude = gpx_attribute(tag, "lon").ok_or(copy::GPX_BROKEN)?;
        let fix =
            Fix::new(latitude, longitude, elevation.unwrap_or(0.0)).ok_or(copy::GPX_BROKEN)?;
        points.push(fix);
        rest = &after[tag_end..];
    }
    Ok(points)
}

/// Latitude and longitude from a map link, or from "lat,lon" text: `geo:3.1,101.6`, a link with
/// `@3.1,101.6,15z`, `q=3.1,101.6` or `ll=3.1,101.6` in it.
pub fn from_link(link: &str) -> Option<Fix> {
    const MARKS: [&str; 5] = ["geo:", "@", "q=", "ll=", "query="];
    let pair = |text: &str| {
        let mut numbers = text.split(',').map(|part| {
            let part = part.trim();
            let end = part
                .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
                .unwrap_or(part.len());
            part[..end].parse::<f64>().ok()
        });
        Fix::new(numbers.next()??, numbers.next()??, 0.0)
    };
    MARKS
        .iter()
        .find_map(|mark| link.split_once(mark).and_then(|(_, rest)| pair(rest)))
        .or_else(|| pair(link.trim()))
}

/// The answer to one position request, at `unix_millis`: one line of twelve comma-separated fields.
/// In order: the word "Fix", the provider, latitude, longitude, altitude, speed in metres per second,
/// accuracy in metres, bearing in degrees, the time, speed accuracy, bearing accuracy, and a clock
/// reading the image ignores.
fn answer(moving: &Moving, unix_millis: u128) -> String {
    let Moving {
        fix: Fix {
            latitude,
            longitude,
            altitude,
        },
        speed,
        bearing,
    } = moving;
    format!(
        "Fix,{PROVIDER},{latitude},{longitude},{altitude},{speed},{ACCURACY_METRES},{bearing},{unix_millis},0,0,0{END_OF_ANSWER}"
    )
}

/// The position to report, shared between whoever sets it and the terminal that answers with it.
#[derive(Default)]
pub struct Location(Mutex<Option<Plan>>);

/// Kilometres an hour to metres a second.
fn metres_per_second(km_per_hour: f64) -> f64 {
    km_per_hour * METRES_PER_KM / SECONDS_PER_HOUR
}

impl Location {
    fn plan(&self) -> std::sync::MutexGuard<'_, Option<Plan>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts the device at a place and keeps it there.
    pub fn set(&self, fix: Fix) {
        *self.plan() = Some(Plan::Still(fix));
    }

    /// Starts walking from wherever the device is, `bearing` degrees from north at `km_per_hour`;
    /// `None` stops where it is. Fails if no position has been set yet.
    pub fn walk(&self, heading: Option<(f64, f64)>) -> Result<(), String> {
        let mut plan = self.plan();
        let now = Instant::now();
        let here = plan.as_ref().ok_or(copy::LOCATION_NOT_SET)?.at(now).fix;
        *plan = Some(match heading {
            Some((bearing, km_per_hour)) => Plan::Walk {
                from: here,
                bearing: bearing.rem_euclid(FULL_TURN_DEGREES),
                speed: metres_per_second(km_per_hour),
                since: now,
            },
            None => Plan::Still(here),
        });
        Ok(())
    }

    /// Follows the route in a GPX file at `km_per_hour`, starting from its first point.
    pub fn follow(&self, path: &Path, km_per_hour: f64) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|err| copy::file(path, &err))?;
        let points = read_gpx(&text)?;
        *self.plan() = Some(Plan::Route {
            points,
            speed: metres_per_second(km_per_hour),
            since: Instant::now(),
        });
        Ok(())
    }

    fn get(&self) -> Option<Moving> {
        self.plan().as_ref().map(|plan| plan.at(Instant::now()))
    }
}

/// Takes what the guest writes to the position terminal and answers each request.
pub struct Requests {
    location: Arc<Location>,
    answers: ConsoleHandle,
    /// The end of the bytes seen so far, in case a request is split across two writes.
    tail: Vec<u8>,
}

impl Requests {
    pub fn new(location: Arc<Location>, answers: ConsoleHandle) -> Self {
        Self {
            location,
            answers,
            tail: Vec::new(),
        }
    }
}

impl Write for Requests {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.tail.extend_from_slice(bytes);
        // The guest's terminal also echoes the answers back; only the request text counts.
        let mut asked = 0;
        while let Some(at) = self
            .tail
            .windows(REQUEST.len())
            .position(|window| window == REQUEST)
        {
            self.tail.drain(..at + REQUEST.len());
            asked += 1;
        }
        let keep = self.tail.len().min(REQUEST.len() - 1);
        self.tail.drain(..self.tail.len() - keep);
        if let (true, Some(moving)) = (asked > 0, self.location.get()) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH);
            let text = answer(&moving, now.map_or(0, |since| since.as_millis()));
            self.answers.send(PORT, text.as_bytes());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use devices::virtio::console::Console;

    use super::*;

    #[test]
    fn answer_is_the_one_line_the_image_parses() {
        let fix = Fix::new(3.139_003, -101.686_855, 60.5).unwrap();
        let moving = Moving {
            fix,
            speed: 1.5,
            bearing: 90.0,
        };
        let text = answer(&moving, 1_795_000_000_123);
        assert_eq!(
            text,
            "Fix,GPS,3.139003,-101.686855,60.5,1.5,3,90,1795000000123,0,0,0\n\n\n\n"
        );
        // The image needs at least twelve fields on the first line.
        assert_eq!(text.lines().next().unwrap().split(',').count(), 12);
    }

    fn close(a: f64, b: f64, within: f64) -> bool {
        (a - b).abs() <= within
    }

    #[test]
    fn walking_and_routes_move_the_position_with_time() {
        let start = Instant::now();
        let origin = Fix::new(0.0, 0.0, 0.0).unwrap();
        // One degree of longitude at the equator is about 111.2 km.
        let east = Plan::Walk {
            from: origin,
            bearing: 90.0,
            speed: 111_195.0,
            since: start,
        };
        let moved = east.at(start + std::time::Duration::from_secs(1));
        assert!(close(moved.fix.longitude, 1.0, 1e-3) && close(moved.fix.latitude, 0.0, 1e-9));

        let a = Fix::new(0.0, 0.0, 0.0).unwrap();
        let b = Fix::new(0.0, 1.0, 100.0).unwrap();
        let c = Fix::new(1.0, 1.0, 100.0).unwrap();
        let (leg, bearing) = distance_and_bearing(&a, &b);
        assert!(close(leg, 111_195.0, 1.0) && close(bearing, 90.0, 1e-9));
        let route = Plan::Route {
            points: vec![a, b, c],
            speed: leg,
            since: start,
        };
        let half = route.at(start + std::time::Duration::from_millis(500));
        assert!(close(half.fix.longitude, 0.5, 1e-3) && close(half.fix.altitude, 50.0, 1e-6));
        assert!(close(half.bearing, 90.0, 1e-3));
        let second_leg = route.at(start + std::time::Duration::from_millis(1500));
        assert!(close(second_leg.fix.latitude, 0.5, 1e-3) && close(second_leg.bearing, 0.0, 1e-3));
        // Past the end it waits at the last point.
        let after = route.at(start + std::time::Duration::from_secs(10));
        assert_eq!((after.fix, after.speed), (c, 0.0));
    }

    #[test]
    fn walking_needs_a_position_and_stopping_keeps_where_it_got_to() {
        let location = Location::default();
        assert_eq!(
            location.walk(Some((0.0, 5.0))),
            Err(copy::LOCATION_NOT_SET.to_owned())
        );
        location.set(Fix::new(1.0, 2.0, 3.0).unwrap());
        location.walk(Some((370.0, 5.0))).unwrap();
        location.walk(None).unwrap();
        let held = location.get().unwrap();
        assert_eq!(held.speed, 0.0);
        assert!(held.fix.latitude >= 1.0 && close(held.fix.longitude, 2.0, 1e-6));
    }

    #[test]
    fn gpx_points_are_read_in_order_with_their_elevation() {
        let gpx = r#"<?xml version="1.0"?>
            <gpx><trk><trkseg>
              <trkpt lat="3.1390" lon='101.6869'><ele>60</ele><time>x</time></trkpt>
              <trkpt lon="101.6870" lat="3.1391"/>
            </trkseg></trk></gpx>"#;
        let points = read_gpx(gpx).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!((points[0].latitude, points[0].altitude), (3.139, 60.0));
        assert_eq!((points[1].longitude, points[1].altitude), (101.687, 0.0));
        assert_eq!(read_gpx("<gpx></gpx>"), Err(copy::GPX_EMPTY.to_owned()));
        // Waypoints written before a track are not part of its route.
        let mixed = r#"<wpt lat="9" lon="9"/><trk><trkpt lat="1" lon="2"/></trk>"#;
        assert_eq!(read_gpx(mixed).unwrap().len(), 1);
        assert_eq!(read_gpx(mixed).unwrap()[0].latitude, 1.0);
        // Characters that are not plain quotes are refused, not a crash.
        assert_eq!(
            read_gpx("<trkpt lat=\u{201c}1\u{201d} lon=\u{201c}2\u{201d}>"),
            Err(copy::GPX_BROKEN.to_owned())
        );
        assert_eq!(
            read_gpx("<trkpt lat=\u{e9}"),
            Err(copy::GPX_BROKEN.to_owned())
        );
        assert_eq!(
            read_gpx(r#"<trkpt lat="x" lon="1">"#),
            Err(copy::GPX_BROKEN.to_owned())
        );
        assert_eq!(
            read_gpx(r#"<trkpt lat="95" lon="1">"#),
            Err(copy::GPX_BROKEN.to_owned())
        );
    }

    #[test]
    fn map_links_and_plain_pairs_give_a_place() {
        let at = |link: &str| from_link(link).map(|fix| (fix.latitude, fix.longitude));
        assert_eq!(at("geo:3.139,101.6869?z=15"), Some((3.139, 101.6869)));
        assert_eq!(
            at("https://www.google.com/maps/@3.139,101.6869,15z"),
            Some((3.139, 101.6869))
        );
        assert_eq!(
            at("https://maps.google.com/?q=-33.86,151.21"),
            Some((-33.86, 151.21))
        );
        assert_eq!(
            at("https://www.openstreetmap.org/?mlat=1#map=5/1/2&ll=1.5,2.5"),
            Some((1.5, 2.5))
        );
        assert_eq!(at("48.8584, 2.2945"), Some((48.8584, 2.2945)));
        assert_eq!(at("https://example.com/nowhere"), None);
        assert_eq!(at("geo:95,0"), None);
    }

    #[test]
    fn positions_that_are_not_on_earth_are_refused() {
        assert!(Fix::new(90.1, 0.0, 0.0).is_none());
        assert!(Fix::new(0.0, -180.5, 0.0).is_none());
        assert!(Fix::new(f64::NAN, 0.0, 0.0).is_none());
        assert!(Fix::new(0.0, 0.0, f64::INFINITY).is_none());
        assert!(Fix::new(-90.0, 180.0, -400.0).is_some());
    }

    #[test]
    fn requests_are_answered_only_once_a_position_is_set() {
        let (console, answers) = Console::new((0..=PORT).map(|_| None).collect());
        let location = Arc::new(Location::default());
        let mut requests = Requests::new(Arc::clone(&location), answers);
        requests.write_all(REQUEST).unwrap();
        assert_eq!(console.pending(PORT), 0);

        location.set(Fix::new(1.0, 2.0, 3.0).unwrap());
        // A request split across writes, with echoed text around it.
        requests.write_all(b"Fix,GPS,echo\nCMD_GET_LO").unwrap();
        assert_eq!(console.pending(PORT), 0);
        requests.write_all(b"CATION").unwrap();
        let one_answer = console.pending(PORT);
        assert!(one_answer > 0);
        // Two requests in one write still get one answer: the position is the same.
        requests
            .write_all(b"CMD_GET_LOCATIONCMD_GET_LOCATION")
            .unwrap();
        assert_eq!(console.pending(PORT), 2 * one_answer);
        assert!(requests.tail.len() < REQUEST.len());
    }
}
