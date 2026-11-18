//! Native image argument and model preflight. This module does not launch a child.
use oxa_config::{Dirs, Settings};
use oxa_proto::{codes, OxaError};
use std::path::{Path, PathBuf};

const DEFAULT_SIZE: (u32, u32) = (512, 512);
const DEFAULT_STEPS: u32 = 4;
// The current oxi latent grid requires both sides to be divisible by 16.
const LATENT_FACTOR: u32 = 16;

pub(crate) struct ImageRequest {
    pub prompt: String,
    pub output: PathBuf,
    pub size: (u32, u32),
    pub seed: Option<u64>,
    pub steps: u32,
}

pub(crate) struct ImageWeights {
    pub diffusion: PathBuf,
    pub text_encoder: PathBuf,
    pub vae: PathBuf,
}

fn usage(message: &'static str) -> OxaError {
    OxaError::new(codes::USAGE, message)
}

fn size(value: &str) -> Result<(u32, u32), OxaError> {
    let invalid = || usage("Image size must be WxH with positive dimensions divisible by 16.");
    let (width, height) = value.split_once('x').ok_or_else(invalid)?;
    let dimension = |value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0 && n % LATENT_FACTOR == 0)
            .ok_or_else(invalid)
    };
    let width = dimension(width)?;
    let height = dimension(height)?;
    width.checked_mul(height).ok_or_else(invalid)?;
    Ok((width, height))
}

fn parse(args: &[String]) -> Result<ImageRequest, OxaError> {
    let mut prompt = None;
    let mut output = None;
    let mut dimensions = None;
    let mut seed = None;
    let mut steps = None;
    let mut parse_options = true;
    let mut words = args.iter().map(String::as_str);
    while let Some(word) = words.next() {
        if parse_options && word == "--" {
            parse_options = false;
            continue;
        }
        if parse_options && word.starts_with("--") {
            if !matches!(word, "--out" | "--size" | "--seed" | "--steps") {
                return Err(usage("Unknown image option."));
            }
            let value = words
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or_else(|| usage("Image options require a value."))?;
            match word {
                "--out" if output.is_none() => {
                    let path = PathBuf::from(value);
                    if !path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
                    {
                        return Err(usage("Image output must have a .png extension."));
                    }
                    output = Some(path);
                }
                "--size" if dimensions.is_none() => dimensions = Some(size(value)?),
                "--seed" if seed.is_none() => {
                    seed = Some(
                        value
                            .parse()
                            .map_err(|_| usage("Image seed must be an unsigned 64-bit integer."))?,
                    );
                }
                "--steps" if steps.is_none() => {
                    steps = Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| usage("Image steps must be a positive integer."))?,
                    );
                }
                _ => return Err(usage("Image options can be supplied only once.")),
            }
        } else if prompt.replace(word).is_some() {
            return Err(usage("Supply one quoted image prompt."));
        }
    }
    let prompt = prompt
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| usage("An image prompt is required."))?;
    Ok(ImageRequest {
        prompt: prompt.to_owned(),
        output: output.unwrap_or_else(|| PathBuf::from("image.png")),
        size: dimensions.unwrap_or(DEFAULT_SIZE),
        seed,
        steps: steps.unwrap_or(DEFAULT_STEPS),
    })
}

fn missing_weights() -> OxaError {
    OxaError::new(
        codes::NOT_FOUND,
        "Native image model or weights are missing. See oxc/docs/DEPENDENCIES.md#image-assets for the pinned FLUX.2 klein diffusion model, text encoder and VAE downloads.",
    )
    .with_hint("Set oxa.image.diffusion, oxa.image.textEncoder and oxa.image.vae to absolute file paths in user settings.")
}

pub(crate) fn preflight(
    args: &[String],
    dirs: &Dirs,
    cwd: &Path,
) -> Result<(ImageRequest, ImageWeights), OxaError> {
    let request = parse(args)?;
    // The project cannot select weights or opt into the image module.
    let user = Settings::load(dirs, cwd, false);
    if !user.bool("oxa.image.enabled", false) {
        return Err(OxaError::new(
            codes::INVALID,
            "Native image generation is disabled. Set oxa.image.enabled to true in user settings.",
        ));
    }
    let weight = |key| {
        user.string(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_file())
            .ok_or_else(missing_weights)
    };
    let weights = ImageWeights {
        diffusion: weight("oxa.image.diffusion")?,
        text_encoder: weight("oxa.image.textEncoder")?,
        vae: weight("oxa.image.vae")?,
    };
    Ok((request, weights))
}
