//! Fractional order keys compatible with the classic a0/Zz Base-62 format.
//!
//! Algorithm and reference fixtures adapted from rocicorp/fractional-indexing
//! (CC0), based on David Greenspan's fractional indexing implementation:
//! https://github.com/rocicorp/fractional-indexing

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrderScope {
    Tasks {
        project_id: String,
        parent_id: Option<String>,
        section_id: Option<String>,
    },
    Projects {
        parent_id: Option<String>,
        workspace_id: Option<String>,
        folder_id: Option<String>,
    },
    Sections {
        project_id: String,
    },
    Filters {
        workspace_id: Option<String>,
    },
}

impl OrderScope {
    pub fn allocation_scope(&self) -> Self {
        match self {
            Self::Projects {
                workspace_id: Some(workspace),
                ..
            } => Self::Projects {
                parent_id: None,
                workspace_id: Some(workspace.clone()),
                folder_id: None,
            },
            _ => self.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPlan {
    pub scope: OrderScope,
    pub positions: Vec<crate::model::TaskOrder>,
    /// Full plans permute known slots, retaining hidden/unlisted resources.
    /// A single-position plan inserts one resource at a remembered ordinal.
    pub full: bool,
    #[serde(default)]
    pub resolve_scopes: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", content = "index", rename_all = "snake_case")]
pub enum Placement {
    Top,
    Bottom,
    Index(i64),
    Legacy(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyWrite {
    pub command: String,
    pub id: String,
    pub field: String,
    pub key: String,
}

fn integer_length(head: u8) -> Result<usize> {
    match head {
        b'a'..=b'z' => Ok((head - b'a') as usize + 2),
        b'A'..=b'Z' => Ok((b'Z' - head) as usize + 2),
        _ => bail!("Unsupported Todoist ordering key format"),
    }
}

fn integer(key: &str) -> Result<&str> {
    ensure!(
        key.is_ascii() && !key.is_empty(),
        "Invalid Todoist ordering key"
    );
    let length = integer_length(key.as_bytes()[0])?;
    ensure!(
        key.len() >= length && key.as_bytes()[1..].iter().all(|byte| DIGITS.contains(byte)),
        "Invalid Todoist ordering key"
    );
    ensure!(
        key.len() == length || !key.ends_with('0'),
        "Invalid Todoist ordering key fraction"
    );
    ensure!(
        key != format!("A{}", "0".repeat(26)),
        "Reserved minimum ordering key"
    );
    Ok(&key[..length])
}

fn adjacent_integer(value: &str, increment: bool) -> Option<String> {
    let mut bytes = value.as_bytes().to_vec();
    for index in (1..bytes.len()).rev() {
        let digit = DIGITS.iter().position(|byte| *byte == bytes[index])?;
        if increment && digit < 61 {
            bytes[index] = DIGITS[digit + 1];
            return String::from_utf8(bytes).ok();
        }
        if !increment && digit > 0 {
            bytes[index] = DIGITS[digit - 1];
            return String::from_utf8(bytes).ok();
        }
        bytes[index] = if increment { b'0' } else { b'z' };
    }
    let head = bytes[0];
    if increment {
        if head == b'z' {
            return None;
        }
        if head == b'Z' {
            return Some("a0".into());
        }
        bytes[0] += 1;
        if bytes[0] > b'a' {
            bytes.push(b'0');
        } else {
            bytes.pop();
        }
    } else {
        if head == b'A' {
            return None;
        }
        if head == b'a' {
            return Some("Zz".into());
        }
        bytes[0] -= 1;
        if bytes[0] >= b'a' {
            bytes.pop();
        } else {
            bytes.push(b'z');
        }
    }
    String::from_utf8(bytes).ok()
}

fn midpoint(mut lower: &[u8], mut upper: Option<&[u8]>) -> String {
    let mut result = Vec::new();
    loop {
        if let Some(high) = upper {
            let mut common = 0;
            while common < high.len() && lower.get(common).copied().unwrap_or(b'0') == high[common]
            {
                common += 1;
            }
            if common > 0 {
                result.extend_from_slice(&high[..common]);
                lower = &lower[common.min(lower.len())..];
                upper = Some(&high[common..]);
                continue;
            }
        }
        let a = lower
            .first()
            .and_then(|byte| DIGITS.iter().position(|digit| digit == byte))
            .unwrap_or(0);
        let b = upper
            .and_then(|high| high.first())
            .and_then(|byte| DIGITS.iter().position(|digit| digit == byte))
            .unwrap_or(62);
        if b > a + 1 {
            result.push(DIGITS[(a + b).div_ceil(2)]);
            break;
        }
        if let Some(high) = upper
            && high.len() > 1
        {
            result.push(high[0]);
            break;
        }
        result.push(DIGITS[a]);
        lower = lower.get(1..).unwrap_or_default();
        upper = None;
    }
    String::from_utf8(result).expect("Base-62 ASCII")
}

pub fn between(lower: Option<&str>, upper: Option<&str>) -> Result<String> {
    if let Some(key) = lower {
        integer(key)?;
    }
    if let Some(key) = upper {
        integer(key)?;
    }
    ensure!(
        !matches!((lower, upper), (Some(a), Some(b)) if a >= b),
        "Ordering key interval is not increasing"
    );
    let key = match (lower, upper) {
        (None, None) => "a0".into(),
        (None, Some(b)) => {
            let ib = integer(b)?;
            if ib == format!("A{}", "0".repeat(26)) {
                format!("{ib}{}", midpoint(b"", Some(&b.as_bytes()[ib.len()..])))
            } else if ib.len() < b.len() {
                ib.to_owned()
            } else {
                adjacent_integer(ib, false)
                    .ok_or_else(|| anyhow::anyhow!("Ordering key has no predecessor"))?
            }
        }
        (Some(a), None) => {
            let ia = integer(a)?;
            adjacent_integer(ia, true)
                .unwrap_or_else(|| format!("{ia}{}", midpoint(&a.as_bytes()[ia.len()..], None)))
        }
        (Some(a), Some(b)) => {
            let ia = integer(a)?;
            let ib = integer(b)?;
            if ia == ib {
                format!(
                    "{ia}{}",
                    midpoint(&a.as_bytes()[ia.len()..], Some(&b.as_bytes()[ib.len()..]))
                )
            } else if let Some(next) = adjacent_integer(ia, true).filter(|next| next.as_str() < b) {
                next
            } else {
                format!("{ia}{}", midpoint(&a.as_bytes()[ia.len()..], None))
            }
        }
    };
    ensure!(
        lower.is_none_or(|bound| bound < key.as_str())
            && upper.is_none_or(|bound| key.as_str() < bound),
        "Could not allocate ordering key"
    );
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_classic_public_domain_reference_fixtures() {
        for (lower, upper, expected) in [
            (None, None, "a0"),
            (None, Some("a0"), "Zz"),
            (None, Some("Zz"), "Zy"),
            (Some("a0"), None, "a1"),
            (Some("a0"), Some("a1"), "a0V"),
            (Some("a0V"), Some("a1"), "a0l"),
            (Some("Zz"), Some("a0"), "ZzV"),
            (None, Some("Y00"), "Xzzz"),
            (Some("bzz"), None, "c000"),
            (Some("a0"), Some("a0V"), "a0G"),
            (Some("b125"), Some("b129"), "b127"),
            (Some("a0"), Some("a1V"), "a1"),
            (None, Some("b999"), "b99"),
        ] {
            assert_eq!(between(lower, upper).unwrap(), expected);
        }
        for invalid in ["a00", "0", "é", "a0!", "A00000000000000000000000000"] {
            assert!(between(Some(invalid), None).is_err());
        }
    }
    #[test]
    fn repeated_insertions_remain_unique_and_lexicographically_ordered() {
        let mut keys: Vec<String> = Vec::new();
        let mut seed = 7_u64;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = seed as usize % (keys.len() + 1);
            let key = between(
                index.checked_sub(1).map(|i| keys[i].as_str()),
                keys.get(index).map(String::as_str),
            )
            .unwrap();
            keys.insert(index, key);
        }
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
