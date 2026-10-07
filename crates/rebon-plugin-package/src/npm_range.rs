use semver::{Prerelease, Version};

// npm compares numeric version components as JavaScript safe integers.
const MAX_COMPONENT: u64 = (1 << 53) - 1;

#[derive(Debug)]
pub(crate) struct NpmRange(Vec<Vec<Bound>>);

#[derive(Debug)]
struct Bound {
    operator: &'static str,
    version: Version,
}

struct Partial {
    version: Version,
    components: usize,
}

impl Partial {
    fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let base = text.split(['-', '+']).next()?;
        let parts: Vec<_> = base.split('.').collect();
        if parts.len() > 3 {
            return None;
        }
        let mut numbers = Vec::new();
        let mut wildcard = false;
        for part in &parts {
            if matches!(*part, "*" | "x" | "X") {
                wildcard = true;
            } else {
                if wildcard
                    || part.is_empty()
                    || (part.len() > 1 && part.starts_with('0'))
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return None;
                }
                let number = part.parse::<u64>().ok()?;
                if number > MAX_COMPONENT {
                    return None;
                }
                numbers.push(number);
            }
        }
        let components = numbers.len();
        if components < 3 && base != text {
            return None;
        }
        let version = if components == 3 {
            Version::parse(text).ok()?
        } else {
            Version::new(
                *numbers.first().unwrap_or(&0),
                *numbers.get(1).unwrap_or(&0),
                0,
            )
        };
        Some(Self {
            version,
            components,
        })
    }

    fn floor(&self, prerelease: bool) -> Version {
        let mut version = self.version.clone();
        if prerelease && version.pre.is_empty() {
            version.pre = Prerelease::new("0").expect("zero is a valid prerelease identifier");
        }
        version
    }

    fn next(&self, component: usize) -> Option<Version> {
        let mut numbers = [self.version.major, self.version.minor, self.version.patch];
        numbers[component] = numbers[component].checked_add(1)?;
        if numbers[component] > MAX_COMPONENT {
            return None;
        }
        numbers[component + 1..].fill(0);
        let mut version = Version::new(numbers[0], numbers[1], numbers[2]);
        version.pre = Prerelease::new("0").expect("zero is a valid prerelease identifier");
        Some(version)
    }
}

impl NpmRange {
    /// The supported npm peer-range grammar, always with includePrerelease.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        text.split("||")
            .map(parse_conjunction)
            .collect::<Option<Vec<_>>>()
            .map(Self)
    }

    pub(crate) fn matches(&self, version: &Version) -> bool {
        self.0.iter().any(|bounds| {
            bounds.iter().all(|bound| {
                let ordering = version.cmp_precedence(&bound.version);
                match bound.operator {
                    "=" => ordering.is_eq(),
                    ">" => ordering.is_gt(),
                    ">=" => !ordering.is_lt(),
                    "<" => ordering.is_lt(),
                    "<=" => !ordering.is_gt(),
                    _ => {
                        unreachable!("range parser only constructs supported comparison operators")
                    }
                }
            })
        })
    }
}

fn parse_conjunction(text: &str) -> Option<Vec<Bound>> {
    let words: Vec<_> = text.split_whitespace().collect();
    let mut bounds = Vec::new();
    if words.len() == 3 && words[1] == "-" {
        let from = Partial::parse(words[0])?;
        let to = Partial::parse(words[2])?;
        if from.components != 0 {
            bounds.push(Bound {
                operator: ">=",
                version: from.floor(true),
            });
        }
        if to.components != 0 {
            let (operator, version) = if to.components == 3 && !to.version.pre.is_empty() {
                ("<=", to.version)
            } else {
                ("<", to.next(to.components - 1)?)
            };
            bounds.push(Bound { operator, version });
        }
        return Some(bounds);
    }
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let (operator, version) = if matches!(word, "=" | ">" | ">=" | "<" | "<=" | "^" | "~") {
            (word, words.next()?)
        } else {
            let end = word
                .find(|c: char| !matches!(c, '=' | '>' | '<' | '^' | '~'))
                .unwrap_or(word.len());
            word.split_at(end)
        };
        expand(operator, Partial::parse(version)?, &mut bounds)?;
    }
    Some(bounds)
}

fn expand(operator: &str, partial: Partial, bounds: &mut Vec<Bound>) -> Option<()> {
    if !matches!(operator, "" | "=" | ">" | ">=" | "<" | "<=" | "^" | "~") {
        return None;
    }
    if partial.components == 0 {
        if matches!(operator, ">" | "<") {
            bounds.push(Bound {
                operator: "<",
                version: partial.floor(true),
            });
        }
        return Some(());
    }
    if matches!(operator, "^" | "~") {
        let component = if operator == "~" {
            usize::from(partial.components > 1)
        } else if partial.version.major > 0 || partial.components == 1 {
            0
        } else if partial.version.minor > 0 || partial.components == 2 {
            1
        } else {
            2
        };
        bounds.push(Bound {
            operator: ">=",
            version: partial
                .floor(operator == "^" && (partial.components < 3 || partial.version.major == 0)),
        });
        bounds.push(Bound {
            operator: "<",
            version: partial.next(component)?,
        });
    } else if partial.components == 3 {
        let operator = match operator {
            "" | "=" => "=",
            ">" => ">",
            ">=" => ">=",
            "<" => "<",
            "<=" => "<=",
            _ => unreachable!(),
        };
        bounds.push(Bound {
            operator,
            version: partial.version,
        });
    } else {
        match operator {
            "" | "=" => {
                bounds.push(Bound {
                    operator: ">=",
                    version: partial.floor(true),
                });
                bounds.push(Bound {
                    operator: "<",
                    version: partial.next(partial.components - 1)?,
                });
            }
            ">" => bounds.push(Bound {
                operator: ">=",
                version: partial.next(partial.components - 1)?,
            }),
            "<=" => bounds.push(Bound {
                operator: "<",
                version: partial.next(partial.components - 1)?,
            }),
            ">=" => bounds.push(Bound {
                operator: ">=",
                version: partial.floor(true),
            }),
            "<" => bounds.push(Bound {
                operator: "<",
                version: partial.floor(true),
            }),
            _ => unreachable!(),
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_include_prerelease_golden() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/npm-range.json")).unwrap();
        assert_eq!(fixture["generator"], "npm semver 7.7.2");
        assert_eq!(fixture["includePrerelease"], true);
        let versions: Vec<_> = fixture["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| Version::parse(value.as_str().unwrap()).unwrap())
            .collect();
        for row in fixture["cases"].as_array().unwrap() {
            let input = row[0].as_str().unwrap();
            let range = NpmRange::parse(input).unwrap();
            let expected = row[1].as_str().unwrap();
            assert_eq!(expected.len(), versions.len());
            for (version, expected) in versions.iter().zip(expected.bytes()) {
                assert_eq!(
                    range.matches(version),
                    expected == b'1',
                    "{input} with {version}"
                );
            }
        }
    }

    #[test]
    fn unsupported_syntax_is_refused_even_after_a_matching_alternative() {
        for text in [
            "workspace:*",
            "latest",
            "git+https://example.com/sdk",
            "1.2.3, 2.0.0",
            "^",
            "!=4",
            "4.*.1",
            "4.01",
            "4.0.0.0",
            "9007199254740992",
            "* || broken",
        ] {
            assert!(NpmRange::parse(text).is_none(), "{text}");
        }
    }
}
