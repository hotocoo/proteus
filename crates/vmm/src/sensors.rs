//! The host end of the Android image's sensor service.
//!
//! The image's sensor service talks to a helper outside the guest over two of its terminals: one for
//! requests, one for readings. It will not start, and Android will not finish booting, until its
//! first request ("which sensors do you have?") is answered. Messages in both directions are a
//! header of two little-endian `u32`s, a command number with the top bit marking a response and the
//! payload length, followed by the payload, which is text.

use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use devices::virtio::console::ConsoleHandle;

use crate::config::{
    ROTATION_SPEED, SENSOR_INTERVAL, SHAKE_ACCELERATION, SHAKE_DURATION, SHAKE_HZ,
};
use crate::copy;

/// Terminal the image sends its sensor requests to (`/dev/hvc18`).
pub const CONTROL_PORT: usize = 18;
/// Terminal the image reads sensor readings from (`/dev/hvc19`).
pub const DATA_PORT: usize = 19;

const HEADER_LEN: usize = 8;
/// Set in the first header word of a message that answers a request.
const IS_RESPONSE: u32 = 1 << 31;
/// The command number the image expects on everything the host sends about sensors.
const UPDATE_HAL: u32 = 2;
/// Requests are short commands; a longer length means the stream is not what it should be.
const MAX_PAYLOAD: usize = 256;
const LIST_SENSORS: &[u8] = b"list-sensors";
/// A request to switch one sensor on or off: this, the sensor's name, a colon, then `1` or `0`.
const SET_SENSOR: &str = "set:";
const END_OF_MESSAGE: char = '\n';

// The image's numbers for the sensors this machine has; the answer to "list" has one bit for each.
const ACCELERATION: u32 = 0;
const GYROSCOPE: u32 = 1;
const MAGNETIC: u32 = 2;
const TEMPERATURE: u32 = 4;
const PROXIMITY: u32 = 5;
const LIGHT: u32 = 6;
const PRESSURE: u32 = 7;
const HUMIDITY: u32 = 8;
const SENSORS: u32 = 1 << ACCELERATION
    | 1 << GYROSCOPE
    | 1 << MAGNETIC
    | 1 << TEMPERATURE
    | 1 << PROXIMITY
    | 1 << LIGHT
    | 1 << PRESSURE
    | 1 << HUMIDITY;

/// The sensors whose readings change with how the device is held, and their names in messages.
const MOVING: [(u32, &str); 3] = [
    (ACCELERATION, "acceleration"),
    (GYROSCOPE, "gyroscope"),
    (MAGNETIC, "magnetic"),
];
/// Standard gravity, in metres per second squared.
const GRAVITY: f64 = 9.806_65;
/// The Earth's magnetic field in the device's frame when it is held upright, in microtesla.
const MAGNETIC_FIELD: Vector = [0.0, 5.9, -48.4];

type Vector = [f64; 3];
type Matrix = [Vector; 3];

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let cell = |row: usize, column: usize| (0..3).map(|k| a[row][k] * b[k][column]).sum();
    [0, 1, 2].map(|row| [0, 1, 2].map(|column| cell(row, column)))
}

fn apply(matrix: &Matrix, vector: &Vector) -> Vector {
    [0, 1, 2].map(|row| (0..3).map(|k| matrix[row][k] * vector[k]).sum())
}

fn transpose(matrix: &Matrix) -> Matrix {
    [0, 1, 2].map(|row| [0, 1, 2].map(|column| matrix[column][row]))
}

/// The rotation that takes vectors of the world into the device's frame, for a device turned by
/// these angles in degrees about its x, y and z axes. The same convention the image's own tools use.
fn rotation(degrees: &Vector) -> Matrix {
    let [x, y, z] = degrees.map(|angle| (-angle).to_radians());
    let about_x = [
        [1.0, 0.0, 0.0],
        [0.0, x.cos(), -x.sin()],
        [0.0, x.sin(), x.cos()],
    ];
    let about_y = [
        [y.cos(), 0.0, y.sin()],
        [0.0, 1.0, 0.0],
        [-y.sin(), 0.0, y.cos()],
    ];
    let about_z = [
        [z.cos(), -z.sin(), 0.0],
        [z.sin(), z.cos(), 0.0],
        [0.0, 0.0, 1.0],
    ];
    multiply(&about_z, &multiply(&about_y, &about_x))
}

/// Angular velocity, in radians per second, of a device that went from one rotation to another in
/// `seconds`.
fn angular_velocity(before: &Matrix, after: &Matrix, seconds: f64) -> Vector {
    let turn = multiply(before, &transpose(after));
    let trace = turn[0][0] + turn[1][1] + turn[2][2];
    let angle = ((trace - 1.0) / 2.0).clamp(-1.0, 1.0).acos();
    let axis = [
        turn[2][1] - turn[1][2],
        turn[0][2] - turn[2][0],
        turn[1][0] - turn[0][1],
    ];
    let length = axis.iter().map(|part| part * part).sum::<f64>().sqrt();
    // No turn, or a time step of nothing: no velocity. (A half turn in one step also lands here,
    // because its axis cannot be read off this way; the speed limit keeps steps far smaller.)
    if seconds <= 0.0 || length < f64::EPSILON {
        return [0.0; 3];
    }
    axis.map(|part| part / length * angle / seconds)
}

struct Pose {
    /// Where the device is being turned to, and where it is now, in degrees about x, y and z.
    target: Vector,
    current: Vector,
    shaking_since: Option<Instant>,
}

/// How the device is held and moved, and which sensors the guest has switched on.
pub struct Motion {
    pose: Mutex<Pose>,
    /// One bit per sensor the guest wants readings from.
    enabled: AtomicU32,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            pose: Mutex::new(Pose {
                target: [0.0; 3],
                current: [0.0; 3],
                shaking_since: None,
            }),
            enabled: AtomicU32::new(0),
        }
    }
}

impl Motion {
    /// Turns the device to these angles, in degrees about its x, y and z axes. All zero is upright
    /// with the screen facing the user. It gets there at `ROTATION_SPEED`, as a hand would.
    pub fn rotate_to(&self, degrees: Vector) {
        self.pose
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .target = degrees;
    }

    /// Shakes the device from side to side for `SHAKE_DURATION`.
    pub fn shake(&self) {
        let mut pose = self.pose.lock().unwrap_or_else(PoisonError::into_inner);
        pose.shaking_since = Some(Instant::now());
    }

    fn set_enabled(&self, sensor: u32, on: bool) {
        if on {
            self.enabled.fetch_or(1 << sensor, Ordering::Relaxed);
        } else {
            self.enabled.fetch_and(!(1 << sensor), Ordering::Relaxed);
        }
    }

    /// Moves the device for `step` of time and returns the readings of the moving sensors:
    /// acceleration (m/s²), angular velocity (rad/s) and magnetic field (µT).
    fn advance(&self, step: Duration, now: Instant) -> [Vector; 3] {
        let mut pose = self.pose.lock().unwrap_or_else(PoisonError::into_inner);
        let before = rotation(&pose.current);
        let reach = ROTATION_SPEED * step.as_secs_f64();
        let target = pose.target;
        for (current, target) in pose.current.iter_mut().zip(target) {
            *current += (target - *current).clamp(-reach, reach);
        }
        let after = rotation(&pose.current);
        let mut acceleration = apply(&after, &[0.0, GRAVITY, 0.0]);
        match pose
            .shaking_since
            .map(|since| now.saturating_duration_since(since))
        {
            Some(shaken) if shaken < SHAKE_DURATION => {
                let phase = shaken.as_secs_f64() * SHAKE_HZ * std::f64::consts::TAU;
                acceleration[0] += SHAKE_ACCELERATION * phase.sin();
            }
            Some(_) => pose.shaking_since = None,
            None => {}
        }
        [
            acceleration,
            angular_velocity(&before, &after, step.as_secs_f64()),
            apply(&after, &MAGNETIC_FIELD),
        ]
    }

    /// The messages for one step: a reading for each moving sensor the guest has switched on.
    fn readings(&self, step: Duration, now: Instant) -> Vec<u8> {
        let enabled = self.enabled.load(Ordering::Relaxed);
        let values = self.advance(step, now);
        let wanted = MOVING.iter().zip(values);
        wanted
            .filter(|((sensor, _), _)| enabled & (1 << sensor) != 0)
            .flat_map(|((_, name), [x, y, z])| {
                message(&format!("{name}:{x}:{y}:{z}{END_OF_MESSAGE}"))
            })
            .collect()
    }
}

/// Sends the guest a reading of every sensor it has switched on, every `SENSOR_INTERVAL`, for as
/// long as `motion` exists.
pub fn spawn_readings(motion: &Arc<Motion>, ports: ConsoleHandle) -> std::io::Result<()> {
    let motion = Arc::downgrade(motion);
    let feed = move || {
        loop {
            std::thread::sleep(SENSOR_INTERVAL);
            let Some(motion) = motion.upgrade() else {
                break;
            };
            let readings = motion.readings(SENSOR_INTERVAL, Instant::now());
            if !readings.is_empty() {
                ports.send(DATA_PORT, &readings);
            }
        }
    };
    std::thread::Builder::new()
        .name(copy::SENSORS_THREAD.into())
        .spawn(feed)?;
    Ok(())
}

/// One message for the guest: a response carrying `text`.
fn message(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + text.len());
    bytes.extend((UPDATE_HAL | IS_RESPONSE).to_le_bytes());
    bytes.extend((text.len() as u32).to_le_bytes());
    bytes.extend(text.as_bytes());
    bytes
}

/// Takes the guest's request bytes as they arrive, and answers the requests that need an answer.
pub struct Requests {
    motion: Arc<Motion>,
    answers: ConsoleHandle,
    /// Bytes of a message that has not arrived completely yet.
    partial: Vec<u8>,
}

impl Requests {
    /// Answers go back to the guest on `CONTROL_PORT` of the console `answers` belongs to.
    pub fn new(motion: Arc<Motion>, answers: ConsoleHandle) -> Self {
        Self {
            motion,
            answers,
            partial: Vec::new(),
        }
    }

    /// The next complete payload, if one has arrived.
    fn next(&mut self) -> Option<Vec<u8>> {
        let header = self.partial.get(..HEADER_LEN)?;
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if len > MAX_PAYLOAD {
            // Not a message this side knows; there is no way to find the next one, so start over.
            self.partial.clear();
            return None;
        }
        if self.partial.len() < HEADER_LEN + len {
            return None;
        }
        let payload = self.partial[HEADER_LEN..HEADER_LEN + len].to_vec();
        self.partial.drain(..HEADER_LEN + len);
        Some(payload)
    }
}

/// The sensor a "switch on" or "switch off" request names, if it is one of the moving ones.
fn switched(request: &[u8]) -> Option<(u32, bool)> {
    let text = std::str::from_utf8(request).ok()?;
    let (name, state) = text.strip_prefix(SET_SENSOR)?.rsplit_once(':')?;
    let (sensor, _) = MOVING.iter().find(|(_, known)| *known == name)?;
    // The state may be followed by a line end or a terminator.
    let on = state.trim_end_matches(|c: char| !c.is_ascii_digit()) != "0";
    Some((*sensor, on))
}

impl Write for Requests {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.partial.extend_from_slice(bytes);
        while let Some(request) = self.next() {
            log::debug!(
                "{}",
                copy::sensor_request(&String::from_utf8_lossy(&request))
            );
            if request.starts_with(LIST_SENSORS) {
                let answer = message(&format!("{SENSORS}{END_OF_MESSAGE}"));
                self.answers.send(CONTROL_PORT, &answer);
            } else if let Some((sensor, on)) = switched(&request) {
                self.motion.set_enabled(sensor, on);
            }
            // Anything else (a reporting rate, the time) needs no answer and changes nothing here.
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

    fn requests(console_ports: usize) -> (Console, Arc<Motion>, Requests) {
        let (console, answers) = Console::new((0..console_ports).map(|_| None).collect());
        let motion = Arc::new(Motion::default());
        let requests = Requests::new(Arc::clone(&motion), answers);
        (console, motion, requests)
    }

    fn close(a: &Vector, b: &Vector) -> bool {
        a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-6)
    }

    const STEP: Duration = Duration::from_millis(20);

    #[test]
    fn upright_device_feels_gravity_along_its_y_axis_and_is_not_turning() {
        let motion = Motion::default();
        let [acceleration, turning, magnetic] = motion.advance(STEP, Instant::now());
        assert!(close(&acceleration, &[0.0, GRAVITY, 0.0]));
        assert!(close(&turning, &[0.0; 3]));
        assert!(close(&magnetic, &MAGNETIC_FIELD));
    }

    #[test]
    fn device_laid_flat_feels_gravity_along_its_z_axis() {
        // Tipped back a quarter turn about x: the screen faces up.
        let flat = apply(&rotation(&[-90.0, 0.0, 0.0]), &[0.0, GRAVITY, 0.0]);
        assert!(close(&flat, &[0.0, 0.0, GRAVITY]), "{flat:?}");
        // A quarter turn about z puts gravity on x: the device is on its side.
        let side = apply(&rotation(&[0.0, 0.0, 90.0]), &[0.0, GRAVITY, 0.0]);
        assert!(close(&side, &[GRAVITY, 0.0, 0.0]), "{side:?}");
    }

    #[test]
    fn turning_takes_time_and_shows_on_the_gyroscope_until_it_is_done() {
        let motion = Motion::default();
        motion.rotate_to([0.0, 0.0, 90.0]);
        let now = Instant::now();
        let [_, turning, _] = motion.advance(STEP, now);
        // One step at the speed limit about z, and nothing about the other axes.
        let expected = ROTATION_SPEED.to_radians();
        assert!((turning[2].abs() - expected).abs() < 1e-6, "{turning:?}");
        assert!(turning[0].abs() < 1e-9 && turning[1].abs() < 1e-9);
        let reached = (ROTATION_SPEED * STEP.as_secs_f64()).min(90.0);
        assert!((motion.pose.lock().unwrap().current[2] - reached).abs() < 1e-9);

        // Long enough to arrive; after that the device is at rest on its side.
        for _ in 0..(90.0 / reached) as usize + 1 {
            motion.advance(STEP, now);
        }
        let [acceleration, turning, _] = motion.advance(STEP, now);
        assert!(
            close(&acceleration, &[GRAVITY, 0.0, 0.0]),
            "{acceleration:?}"
        );
        assert!(close(&turning, &[0.0; 3]));
    }

    #[test]
    fn shaking_moves_the_x_axis_for_a_while_and_then_stops() {
        let motion = Motion::default();
        motion.shake();
        let started = motion.pose.lock().unwrap().shaking_since.unwrap();
        // A quarter of a shake period in: the push is at its strongest.
        let peak = started + Duration::from_secs_f64(0.25 / SHAKE_HZ);
        let [acceleration, _, _] = motion.advance(STEP, peak);
        assert!(
            (acceleration[0] - SHAKE_ACCELERATION).abs() < 1e-6,
            "{acceleration:?}"
        );
        let [acceleration, _, _] = motion.advance(STEP, started + SHAKE_DURATION);
        assert!(close(&acceleration, &[0.0, GRAVITY, 0.0]));
        assert!(motion.pose.lock().unwrap().shaking_since.is_none());
    }

    #[test]
    fn readings_are_sent_only_for_sensors_the_guest_switched_on() {
        let (_console, motion, mut requests) = requests(DATA_PORT + 1);
        assert!(motion.readings(STEP, Instant::now()).is_empty());
        requests.write_all(&request(b"set:acceleration:1")).unwrap();
        requests.write_all(&request(b"set:light:1")).unwrap();
        let expected = message(&format!("acceleration:0:{GRAVITY}:0\n"));
        assert_eq!(motion.readings(STEP, Instant::now()), expected);
        requests
            .write_all(&request(b"set:acceleration:0\n"))
            .unwrap();
        assert!(motion.readings(STEP, Instant::now()).is_empty());
    }

    fn request(text: &[u8]) -> Vec<u8> {
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend((text.len() as u32).to_le_bytes());
        bytes.extend(text);
        bytes
    }

    #[test]
    fn answer_names_the_sensors_as_a_response_message() {
        assert_eq!(
            message("503\n"),
            [2, 0, 0, 0x80, 4, 0, 0, 0, b'5', b'0', b'3', b'\n']
        );
        assert_eq!(SENSORS, 503);
    }

    #[test]
    fn list_request_is_answered_however_its_bytes_are_split() {
        let (console, _motion, mut requests) = requests(CONTROL_PORT + 1);
        let mut stream = request(b"set:acceleration:1");
        stream.extend(request(LIST_SENSORS));
        let (first, rest) = stream.split_at(HEADER_LEN + 3);
        requests.write_all(first).unwrap();
        assert_eq!(console.pending(CONTROL_PORT), 0);
        requests.write_all(rest).unwrap();
        assert_eq!(console.pending(CONTROL_PORT), message("503\n").len());
        assert!(requests.partial.is_empty());
    }

    #[test]
    fn a_length_no_request_has_resets_the_stream() {
        let (console, _motion, mut requests) = requests(CONTROL_PORT + 1);
        let mut garbage = 0u32.to_le_bytes().to_vec();
        garbage.extend(u32::MAX.to_le_bytes());
        requests.write_all(&garbage).unwrap();
        assert!(requests.partial.is_empty());
        requests.write_all(&request(LIST_SENSORS)).unwrap();
        assert_ne!(console.pending(CONTROL_PORT), 0);
    }
}
