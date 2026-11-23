//! The `ls -l` style line of a directory entry.
//!
//! Version 3 directory listings carry one text line per entry, and some
//! clients, OpenSSH's `sftp` among them, print it as it is.

use bytes::{BufMut, Bytes, BytesMut};

use crate::wire::{Attributes, FileType};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 to (year, month 1-12, day 1-31), proleptic
/// Gregorian.
fn civil(days: i64) -> (i64, usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as usize, day)
}

/// Formats `name` and its attributes like a line of `ls -l`. Missing
/// attributes show as `?`-free neutral values (0, `Jan  1  1970`). Times
/// are UTC.
pub fn format_long_name(name: &[u8], attrs: &Attributes) -> Bytes {
    let kind = match attrs.file_type() {
        Some(FileType::Dir) => 'd',
        Some(FileType::Symlink) => 'l',
        Some(FileType::File) => '-',
        _ => '?',
    };
    let mode = attrs.permissions.unwrap_or(0);
    let mut perms = String::with_capacity(10);
    perms.push(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        perms.push(if bits & 4 != 0 { 'r' } else { '-' });
        perms.push(if bits & 2 != 0 { 'w' } else { '-' });
        perms.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    let (uid, gid) = attrs.owner.map_or((0, 0), |o| (o.uid, o.gid));
    let size = attrs.size.unwrap_or(0);
    let date = match attrs.times {
        Some(t) => {
            let secs = i64::from(t.modified);
            let (_, month, day) = civil(secs.div_euclid(86_400));
            let in_day = secs.rem_euclid(86_400);
            format!(
                "{} {:>2} {:02}:{:02}",
                MONTHS[month - 1],
                day,
                in_day / 3600,
                in_day % 3600 / 60
            )
        }
        None => "Jan  1  1970".to_owned(),
    };
    let head = format!("{perms} {:>3} {uid:<8} {gid:<8} {size:>8} {date} ", 1);
    let mut out = BytesMut::with_capacity(head.len() + name.len());
    out.put_slice(head.as_bytes());
    out.put_slice(name);
    out.freeze()
}
