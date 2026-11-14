use std::{
    collections::HashSet,
    fmt::{Display, Formatter},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Registry {
    DockerHub,
    Ghcr,
}

impl Registry {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DockerHub => "dockerhub",
            Self::Ghcr => "ghcr",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Image {
    pub registry: Registry,
    pub owner: String,
    pub name: String,
}

impl Display for Image {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}/{}", self.registry.as_str(), self.owner, self.name)
    }
}

pub fn parse_specs(text: &str) -> Result<Vec<Image>, String> {
    let mut images = Vec::new();
    let mut seen = HashSet::new();

    for spec in text.split(|character: char| character == ',' || character.is_whitespace()) {
        if spec.is_empty() {
            continue;
        }

        let image = parse_spec(spec)?;
        if seen.insert(image.clone()) {
            images.push(image);
        }
    }

    Ok(images)
}

fn parse_spec(spec: &str) -> Result<Image, String> {
    let (registry_alias, path) = spec
        .split_once(':')
        .ok_or_else(|| format!("expected <registry>:<owner>/<image>, got {spec:?}"))?;
    let registry = match registry_alias.trim().to_ascii_lowercase().as_str() {
        "ghcr" | "ghcr.io" | "github" => Registry::Ghcr,
        "dockerhub" | "docker" | "hub" | "docker.io" | "hub.docker.com" => Registry::DockerHub,
        _ => {
            return Err(format!(
                "unknown registry {:?} in {spec:?} (use ghcr or dockerhub)",
                registry_alias
            ));
        }
    };

    let path = path.trim().trim_matches('/');
    let (owner, name) = match path.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() => {
            (owner.to_owned(), name.to_owned())
        }
        Some(_) => return Err(format!("bad image spec {spec:?}")),
        None if registry == Registry::DockerHub && !path.is_empty() => {
            ("library".to_owned(), path.to_owned())
        }
        None if registry == Registry::Ghcr => {
            return Err(format!("ghcr images need owner/image, got {spec:?}"));
        }
        None => return Err(format!("bad image spec {spec:?}")),
    };

    Ok(Image {
        registry,
        owner,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::{Image, Registry, parse_specs};

    #[test]
    fn parses_aliases_official_images_and_deduplicates() {
        let images =
            parse_specs("ghcr.io:linuxserver/sonarr, docker:nginx\ndockerhub:library/nginx")
                .unwrap();

        assert_eq!(
            images,
            vec![
                Image {
                    registry: Registry::Ghcr,
                    owner: "linuxserver".into(),
                    name: "sonarr".into(),
                },
                Image {
                    registry: Registry::DockerHub,
                    owner: "library".into(),
                    name: "nginx".into(),
                },
            ]
        );
    }

    #[test]
    fn rejects_invalid_and_incomplete_specs() {
        assert!(parse_specs("unknown:owner/image").is_err());
        assert!(parse_specs("ghcr:sonarr").is_err());
        assert!(parse_specs("dockerhub:/").is_err());
    }
}
