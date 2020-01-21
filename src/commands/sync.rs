use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use png;
use sha2::{Digest, Sha256};
use sheep::{self, InputSprite, MaxrectsOptions, MaxrectsPacker};
use snafu::ResultExt;
use walkdir::WalkDir;

use crate::{
    asset_name::AssetName,
    auth_cookie::get_auth_cookie,
    codegen::{AssetUrlTemplate, UrlAndSliceTemplate},
    data::{CodegenKind, Config, ImageSlice, InputConfig, InputManifest, Manifest},
    dpi_scale::dpi_scale_for_path,
    options::{GlobalOptions, SyncOptions, SyncTarget},
    roblox_web_api::{ImageUploadData, RobloxApiClient},
    spritesheet::{OutputFormat, Spritesheet},
};

use self::error::Error;
pub use self::error::Error as SyncError;

pub fn sync(global: GlobalOptions, options: SyncOptions) -> Result<(), Error> {
    let fuzzy_config_path = match options.config_path {
        Some(v) => v,
        None => env::current_dir().context(error::CurrentDir)?,
    };

    let mut api_client = global
        .auth
        .or_else(get_auth_cookie)
        .map(RobloxApiClient::new);

    let mut session = SyncSession::new(&fuzzy_config_path)?;

    session.discover_configs()?;
    session.discover_inputs()?;

    match options.target {
        SyncTarget::Roblox => {
            let api_client = api_client.as_mut().ok_or(Error::NoAuth)?;
            let mut strategy = RobloxUploadStrategy { api_client };

            session.sync(&mut strategy)?;
        }
        SyncTarget::ContentFolder => {
            let mut strategy = ContentUploadStrategy {};

            session.sync(&mut strategy)?;
        }
    }

    session.write_manifest()?;
    session.codegen()?;

    Ok(())
}

/// A sync session holds all of the state for a single run of the 'tarmac sync'
/// command.
#[derive(Debug)]
struct SyncSession {
    /// The set of all configs known by the sync session.
    ///
    /// This list is always at least one element long. The first entry is the
    /// root config where the sync session was started; use
    /// SyncSession::root_config to retrieve it.
    configs: Vec<Config>,

    /// The manifest file that was present as of the beginning of the sync
    /// operation.
    original_manifest: Manifest,

    /// All of the inputs discovered so far in the current sync.
    inputs: HashMap<AssetName, SyncInput>,
}

#[derive(Debug)]
struct SyncInput {
    /// The path on disk to the file containing this input.
    path: PathBuf,

    /// An index into SyncSession::configs representing the config that applies
    /// to this input.
    config: InputConfig,

    /// The content hash associated with the input, if we've calculated it.
    hash: Option<String>,

    /// The asset ID of this input the last time it was uploaded.
    id: Option<u64>,

    /// If the asset is an image that was packed into a spritesheet, contains
    /// the portion of the uploaded image that contains this input.
    slice: Option<ImageSlice>,
}

impl SyncInput {
    pub fn matches_manifest(&self, manifest: &InputManifest) -> bool {
        self.hash.is_some()
            && self.hash == manifest.hash
            && self.config.packable == manifest.packable
            && self.config.codegen == manifest.codegen
    }
}

impl SyncSession {
    fn new(fuzzy_config_path: &Path) -> Result<Self, Error> {
        log::trace!("Starting new sync session");

        let root_config =
            Config::read_from_folder_or_file(&fuzzy_config_path).context(error::Config)?;

        log::trace!("Starting from config \"{}\"", root_config.name);

        let original_manifest = match Manifest::read_from_folder(root_config.folder()) {
            Ok(manifest) => manifest,
            Err(err) if err.is_not_found() => Manifest::default(),
            other => other.context(error::Manifest)?,
        };

        Ok(Self {
            configs: vec![root_config],
            original_manifest,
            inputs: HashMap::new(),
        })
    }

    /// The config that this sync session was started from.
    fn root_config(&self) -> &Config {
        &self.configs[0]
    }

    /// Locate all of the configs connected to our root config.
    ///
    /// Tarmac config files can include each other via the `includes` field,
    /// which will search the given path for other config files and use them as
    /// part of the sync.
    fn discover_configs(&mut self) -> Result<(), Error> {
        let mut to_search = VecDeque::new();
        to_search.extend(
            self.root_config()
                .includes
                .iter()
                .map(|include| include.path.clone()),
        );

        while let Some(search_path) = to_search.pop_front() {
            let search_meta =
                fs::metadata(&search_path).context(error::Io { path: &search_path })?;

            if search_meta.is_file() {
                // This is a file that's explicitly named by a config. We'll
                // check that it's a Tarmac config and include it.

                let config = Config::read_from_file(&search_path).context(error::Config)?;

                // Include any configs that this config references.
                to_search.extend(config.includes.iter().map(|include| include.path.clone()));

                self.configs.push(config);
            } else {
                // If this directory contains a config file, we can stop
                // traversing this branch.

                match Config::read_from_folder(&search_path) {
                    Ok(config) => {
                        // We found a config, we're done here.

                        // Append config include paths from this config
                        to_search
                            .extend(config.includes.iter().map(|include| include.path.clone()));

                        self.configs.push(config);
                    }

                    Err(err) if err.is_not_found() => {
                        // We didn't find a config, keep searching down this
                        // branch of the filesystem.

                        let children =
                            fs::read_dir(&search_path).context(error::Io { path: &search_path })?;

                        for entry in children {
                            let entry = entry.context(error::Io { path: &search_path })?;
                            let entry_path = entry.path();

                            // DirEntry has a metadata method, but in the case
                            // of symlinks, it returns metadata about the
                            // symlink and not the file or folder.
                            let entry_meta = fs::metadata(&entry_path)
                                .context(error::Io { path: &entry_path })?;

                            if entry_meta.is_dir() {
                                to_search.push_back(entry_path);
                            }
                        }
                    }

                    err @ Err(_) => {
                        err.context(error::Config)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Find all files on the filesystem referenced as inputs by our configs.
    fn discover_inputs(&mut self) -> Result<(), Error> {
        let inputs = &mut self.inputs;

        // Starting with our root config, iterate over all configs and find all
        // relevant inputs
        for config in &self.configs {
            let config_path = config.folder();

            for input_config in &config.inputs {
                let base_path = config_path.join(input_config.glob.get_prefix());
                log::trace!(
                    "Searching for inputs in '{}' matching '{}'",
                    base_path.display(),
                    input_config.glob,
                );

                let filtered_paths = WalkDir::new(base_path)
                    .into_iter()
                    // TODO: Properly handle WalkDir errors
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        let match_path = entry.path().strip_prefix(config_path).unwrap();
                        input_config.glob.is_match(match_path)
                    });

                for matching in filtered_paths {
                    let name = AssetName::from_paths(config_path, matching.path());
                    log::trace!("Found input {}", name);

                    let already_found = inputs.insert(
                        name,
                        SyncInput {
                            path: matching.into_path(),
                            config: input_config.clone(),
                            hash: None,
                            id: None,
                            slice: None,
                        },
                    );

                    if let Some(existing) = already_found {
                        return Err(Error::OverlappingGlobs {
                            path: existing.path,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn sync<S: UploadStrategy>(&mut self, strategy: &mut S) -> Result<(), Error> {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        struct InputCompatibility {
            packable: bool,
            dpi_scale: u32,
        }

        let mut compatible_input_groups = HashMap::new();

        for (input_name, input) in &self.inputs {
            let compatibility = InputCompatibility {
                packable: input.config.packable,
                dpi_scale: dpi_scale_for_path(&input.path),
            };

            let input_group = compatible_input_groups
                .entry(compatibility)
                .or_insert_with(Vec::new);

            input_group.push(input_name.clone());
        }

        for (compatibility, group) in compatible_input_groups {
            if compatibility.packable {
                let spritesheets = self.pack_images(group)?;

                for (contents, spritesheet) in spritesheets {
                    self.sync_packed_image(strategy, contents, &spritesheet)?;
                }
            } else {
                for input_name in group {
                    let input = self.inputs.get(&input_name).unwrap();

                    log::trace!("Syncing {}", &input_name);

                    if is_image_asset(&input.path) {
                        self.sync_unpackable_image(strategy, &input_name)?;
                    } else {
                        log::warn!("Didn't know what to do with asset {}", input.path.display());
                    }
                }
            }
        }

        // TODO: Clean up output of inputs that were present in the previous
        // sync but are no longer present.

        Ok(())
    }

    fn pack_images(
        &mut self,
        mut input_group: Vec<AssetName>,
    ) -> Result<Vec<(Vec<u8>, Spritesheet)>, SyncError> {
        log::info!("Packing {} compatible images", input_group.len());

        // First, sort the inputs to make sure they're' always processed in a
        // consistent order. This makes sure we avoid generating different
        // spritesheets with the same input sprites.
        input_group.sort();

        // For each asset in this group, compute the hash of its content; we can
        // use this to verify whether or not any of the images have changed
        let mut unchanged = true;
        for asset_name in input_group.iter() {
            let input = self.inputs.get_mut(asset_name).unwrap();
            let path = input.path.as_path();
            let contents = fs::read(path).context(error::Io { path })?;

            input.hash = Some(generate_asset_hash(contents.as_ref()));

            // Once the hash is computed, compare with the manifest's hash to
            // determine if we have any changed inputs
            let input_manifest = self.original_manifest.inputs.get(asset_name);
            let matches_manifest = input_manifest
                .map(|manifest| input.matches_manifest(manifest))
                .unwrap_or(false);

            // If the input in question has an id and matches the input
            // manifest, then we may not need to spritesheet it
            unchanged &= input.id.is_some() && matches_manifest;

            // TODO: Make sure this aligns with the content folder upload
            // strategy; particularly, when we may not have uploaded but have
            // already generated a spritesheet and put it somewhere
        }

        // If none of the input images have changed, we can short-circuit here
        if unchanged {
            log::info!("All input images are unchanged; no need to pack spritesheets");
            return Ok(Vec::new());
        }

        let packable_inputs: Vec<InputSprite> = input_group
            .iter()
            .map(|asset_name| {
                // Build a png decoder for the given file
                let path = self.inputs.get(asset_name).unwrap().path.as_path();
                let image_file = fs::File::open(path).context(error::Io { path })?;
                let decoder = png::Decoder::new(image_file);

                // Get the metadata we need from the image and read its data
                // into a buffer for processing by the sprite packing algorithm
                let (info, mut reader) = decoder.read_info().context(error::PngDecode)?;
                let dimensions = (info.width, info.height);
                let mut bytes = vec![0; info.buffer_size()];
                reader.next_frame(&mut bytes).context(error::PngDecode)?;

                log::trace!(
                    "Processing input {}\n\tDimensions: ({}, {})\n\tBitDepth: {:?}\n\tColorType: {:?}\n\tBytes: {}",
                    path.display(),
                    dimensions.0,
                    dimensions.1,
                    info.bit_depth,
                    info.color_type,
                    bytes.len()
                );

                // The packing algorithm expects to have an alpha channel, so
                // non-RGBA images are unsupported
                // TODO: Transcode images to RGBA if/when possible
                if info.color_type != png::ColorType::RGBA {
                    return Err(SyncError::UnsupportedFormat { path: path.to_path_buf(), format: info.color_type });
                }

                Ok(InputSprite { bytes, dimensions })
            })
            .collect::<Result<_, SyncError>>()?;

        // The Maxrects sprite packing algorithm accepts options for the max
        // width and height of the generated spritesheets
        let dimensions = self.root_config().max_spritesheet_size;
        let opts = MaxrectsOptions::default()
            .max_width(dimensions.0)
            .max_height(dimensions.1);

        // Pack all inputs into one or more spritesheet buckets
        let pack_results = sheep::pack::<MaxrectsPacker>(packable_inputs, 4, opts);

        log::info!("Generated {} spritesheets", pack_results.len());

        pack_results
            .into_iter()
            .map(|result| {
                let spritesheet = sheep::encode::<OutputFormat>(&result, input_group.clone());

                let mut contents = Vec::new();

                let mut encoder =
                    png::Encoder::new(&mut contents, result.dimensions.0, result.dimensions.1);

                encoder.set_color(png::ColorType::RGBA);
                encoder.set_depth(png::BitDepth::Eight);

                // Write out RGBA8 image data
                {
                    let mut output_writer = encoder.write_header().context(error::PngEncode)?;

                    output_writer
                        .write_image_data(&result.bytes)
                        .context(error::PngEncode)?;

                    // On drop, output_writer will write the last chunk of the
                    // PNG file.
                }

                log::trace!("Generated spritesheet:\n{:#?}", spritesheet);

                Ok((contents, spritesheet))
            })
            .collect()
    }

    fn sync_unpackable_image<S: UploadStrategy>(
        &mut self,
        strategy: &mut S,
        input_name: &AssetName,
    ) -> Result<(), Error> {
        let input = self.inputs.get_mut(input_name).unwrap();
        let contents = fs::read(&input.path).context(error::Io { path: &input.path })?;
        let hash = generate_asset_hash(&contents);

        input.hash = Some(hash.clone());

        let upload_data = UploadData {
            name: input_name.clone(),
            contents,
            hash: hash.clone(),
        };

        let id = if let Some(input_manifest) = self.original_manifest.inputs.get(&input_name) {
            // This input existed during our last sync operation. We'll compare
            // the current state with the previous one to see if we need to take
            // action.

            if input_manifest.hash.as_ref() != Some(&hash) {
                // The file's contents have been edited since the last sync.

                log::trace!("Contents changed...");

                strategy.upload(upload_data)?.id
            } else if let Some(prev_id) = input_manifest.id {
                // The file's contents are the same as the previous sync and
                // this image has been uploaded previously.

                if input_manifest.packable != input.config.packable
                    || input_manifest.codegen != input.config.codegen
                {
                    // Only the file's config has changed.
                    //
                    // TODO: We might not need to reupload this image?

                    log::trace!("Config changed...");

                    strategy.upload(upload_data)?.id
                } else {
                    // Nothing has changed, we're good to go!

                    log::trace!("Input is unchanged");

                    prev_id
                }
            } else {
                // This image has never been uploaded, but its hash is present
                // in the manifest.

                log::trace!("Image has never been uploaded...");

                strategy.upload(upload_data)?.id
            }
        } else {
            // This input was added since the last sync, if there was one.

            log::trace!("Image was added since last sync...");

            strategy.upload(upload_data)?.id
        };

        input.id = Some(id);

        Ok(())
    }

    fn sync_packed_image<S: UploadStrategy>(
        &mut self,
        strategy: &mut S,
        contents: Vec<u8>,
        packed_image: &Spritesheet,
    ) -> Result<(), Error> {
        let mut slices = packed_image.slices();
        let (first_asset, _) = slices.next().unwrap();
        let first_input = self.inputs.get(first_asset).unwrap();

        // If there's an existing id that lines up with all of the inputs in the
        // spritesheet, we don't need to upload and we can short-circuit
        let existing_id = first_input.id.and_then(|id| {
            for (asset_name, _) in slices {
                let input = self.inputs.get(asset_name).unwrap();
                if input.id != Some(id) {
                    return None;
                }
            }

            Some(id)
        });

        if let Some(id) = existing_id {
            log::info!("Asset id {} has already been uploaded", id);
            return Ok(());
        }

        // TODO: Do we need to save the hash of entire spritesheet contents so
        // we can avoid re-uploading an identical spritesheet? Or can we get by
        // with the above check?

        let hash = generate_asset_hash(contents.as_ref());

        let upload_data = UploadData {
            name: AssetName::spritesheet(),
            contents,
            hash: hash.clone(),
        };

        let id = strategy.upload(upload_data)?.id;

        // Apply resolved metadata back to the inputs
        for (asset_name, slice) in packed_image.slices() {
            let input = self.inputs.get_mut(asset_name).unwrap();

            input.id = Some(id);
            input.slice = Some(slice.to_owned());
        }

        Ok(())
    }

    fn write_manifest(&self) -> Result<(), Error> {
        log::trace!("Generating new manifest");

        let mut manifest = Manifest::default();

        manifest.inputs = self
            .inputs
            .iter()
            .map(|(name, input)| {
                (
                    name.clone(),
                    InputManifest {
                        hash: input.hash.clone(),
                        id: input.id,
                        slice: input.slice,
                        packable: input.config.packable,
                        codegen: input.config.codegen,
                    },
                )
            })
            .collect();

        manifest
            .write_to_folder(self.root_config().folder())
            .context(error::Manifest)?;

        Ok(())
    }

    fn codegen(&self) -> Result<(), Error> {
        log::trace!("Starting codegen");

        for (input_name, input) in &self.inputs {
            log::trace!(
                "Using codegen '{:?}' for {}",
                input.config.codegen,
                input_name
            );

            match input.config.codegen {
                CodegenKind::None => {}

                CodegenKind::AssetUrl => {
                    if let Some(id) = input.id {
                        let template = AssetUrlTemplate { id };

                        let path = &input.path.with_extension("lua");
                        let mut file = File::create(path).context(error::Io { path })?;
                        write!(&mut file, "{}", template).context(error::Io { path })?;

                        log::trace!("Generated code at {}", path.display());
                    } else {
                        log::trace!("Skipping codegen because this input was not uploaded.");
                    }
                }

                CodegenKind::UrlAndSlice => {
                    if let Some(id) = input.id {
                        let template = UrlAndSliceTemplate {
                            id,
                            slice: input.slice,
                        };

                        let path = &input.path.with_extension("lua");
                        let mut file = File::create(path).context(error::Io { path })?;
                        write!(&mut file, "{}", template).context(error::Io { path })?;

                        log::trace!("Generated code at {}", path.display());
                    } else {
                        log::trace!("Skipping codegen because this input was not uploaded.");
                    }
                }
            }
        }

        Ok(())
    }
}

struct UploadResponse {
    id: u64,
    // TODO: Other asset URL construction information to support content folder
    // shenanigans.
}

struct UploadData {
    name: AssetName,
    contents: Vec<u8>,
    hash: String,
}

trait UploadStrategy {
    fn upload(&mut self, data: UploadData) -> Result<UploadResponse, SyncError>;
}

struct RobloxUploadStrategy<'a> {
    api_client: &'a mut RobloxApiClient,
}

impl<'a> UploadStrategy for RobloxUploadStrategy<'a> {
    fn upload(&mut self, data: UploadData) -> Result<UploadResponse, SyncError> {
        log::info!("Uploading {} to Roblox", &data.name);

        let response = self
            .api_client
            .upload_image(ImageUploadData {
                image_data: Cow::Owned(data.contents),
                name: data.name.as_ref(),
                description: "Uploaded by Tarmac.",
            })
            .expect("Upload failed");

        log::info!(
            "Uploaded {} to ID {}",
            &data.name,
            response.backing_asset_id
        );

        Ok(UploadResponse {
            id: response.backing_asset_id,
        })
    }
}

struct ContentUploadStrategy {
    // TODO: Studio install information
}

impl UploadStrategy for ContentUploadStrategy {
    fn upload(&mut self, _data: UploadData) -> Result<UploadResponse, SyncError> {
        unimplemented!("content folder uploading");
    }
}

fn is_image_asset(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        // TODO: Expand the definition of images?
        Some("png") | Some("jpg") => true,

        _ => false,
    }
}

fn generate_asset_hash(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

mod error {
    use crate::data::{ConfigError, ManifestError};
    use png;
    use snafu::Snafu;
    use std::{io, path::PathBuf};
    use walkdir;

    #[derive(Debug, Snafu)]
    #[snafu(visibility = "pub(super)")]
    pub enum Error {
        #[snafu(display("{}", source))]
        Config {
            source: ConfigError,
        },

        #[snafu(display("{}", source))]
        Manifest {
            source: ManifestError,
        },

        Io {
            path: PathBuf,
            source: io::Error,
        },

        #[snafu(display("couldn't get the current directory of the process"))]
        CurrentDir {
            source: io::Error,
        },

        #[snafu(display("'tarmac sync' requires an authentication cookie"))]
        NoAuth,

        // TODO: Add more detail here and better display
        #[snafu(display("{}", source))]
        WalkDir {
            source: walkdir::Error,
        },

        // TODO: Add more detail here and better display
        #[snafu(display("Path {} was described by more than one glob", path.display()))]
        OverlappingGlobs {
            path: PathBuf,
        },

        #[snafu(display(
            "Input {} has unsupported png format {:?} (requires RGBA format)",
            path.display(),
            format,
        ))]
        UnsupportedFormat {
            path: PathBuf,
            format: png::ColorType,
        },

        // TODO: Add more detail here and better display
        #[snafu(display("{}", source))]
        PngDecode {
            source: png::DecodingError,
        },

        // TODO: Add more detail here and better display
        #[snafu(display("{}", source))]
        PngEncode {
            source: png::EncodingError,
        },
    }
}
