//! GitHub login (username) validation. A login that cannot exist is refused
//! before any request is made, so a playbook typo like `"cadu@x86"` or an
//! empty var fails at `check` with a message instead of a 404 from GitHub.

use rustible_sdk::prelude::*;

/// GitHub's own limit on login length.
const MAX_LEN: usize = 39;

/// Pure: is `login` a legal GitHub username?
///
/// GitHub's rules, as its sign-up form states them: 1 to 39 characters,
/// ASCII letters, digits, and single hyphens; no leading or trailing hyphen
/// and no two hyphens in a row. Case is not significant to GitHub and is not
/// checked here. Organizations follow the same rules, so an org name passes
/// too (and then gets a 404 from the `.keys` endpoint, which only serves
/// users).
///
/// ```
/// use rustible_github::validate_login;
///
/// assert!(validate_login("flipbit03").is_ok());
/// assert!(validate_login("octo-cat").is_ok());
/// assert!(validate_login("").is_err());
/// assert!(validate_login("-cadu").is_err());
/// assert!(validate_login("ca--du").is_err());
/// assert!(validate_login("cadu@x86").is_err());
/// ```
pub fn validate_login(login: &str) -> Result<()> {
    if login.is_empty() {
        bail!("GitHub login is empty");
    }
    if login.len() > MAX_LEN {
        bail!(
            "GitHub login `{login}` is {} characters long; the maximum is {MAX_LEN}",
            login.len()
        );
    }
    if let Some(c) = login
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
    {
        bail!(
            "GitHub login `{login}` contains {c:?}; only ASCII letters, digits, and hyphens are allowed"
        );
    }
    if login.starts_with('-') || login.ends_with('-') {
        bail!("GitHub login `{login}` cannot begin or end with a hyphen");
    }
    if login.contains("--") {
        bail!("GitHub login `{login}` cannot contain two hyphens in a row");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_logins() {
        for ok in [
            "flipbit03",
            "a",
            "A1",
            "octo-cat",
            "x-y-z",
            "0day",
            "ABCDEF",
        ] {
            assert!(validate_login(ok).is_ok(), "{ok}");
        }
        assert!(validate_login(&"a".repeat(39)).is_ok());
    }

    #[test]
    fn rejects_length() {
        assert!(validate_login("").unwrap_err().chain().contains("empty"));
        let long = "a".repeat(40);
        let e = validate_login(&long).unwrap_err().chain();
        assert!(
            e.contains("40 characters") && e.contains("maximum is 39"),
            "{e}"
        );
    }

    #[test]
    fn rejects_hyphen_placement() {
        assert!(
            validate_login("-cadu")
                .unwrap_err()
                .chain()
                .contains("begin or end")
        );
        assert!(
            validate_login("cadu-")
                .unwrap_err()
                .chain()
                .contains("begin or end")
        );
        assert!(
            validate_login("ca--du")
                .unwrap_err()
                .chain()
                .contains("two hyphens")
        );
        assert!(validate_login("-").is_err());
    }

    #[test]
    fn rejects_characters_that_cannot_appear() {
        for bad in [
            "cadu@x86",
            "cadu x86",
            "ca_du",
            "cadu.keys",
            "../flipbit03",
            "flipbit03/",
            "flipbit03?x=1",
            "cadú",
            "flip\nbit",
        ] {
            let e = validate_login(bad).unwrap_err().chain();
            assert!(e.contains("only ASCII letters"), "{bad}: {e}");
        }
    }
}
