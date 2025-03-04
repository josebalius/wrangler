use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use config::{Config, File};

use serde::{Deserialize, Serialize};
use serde_with::rust::string_empty_as_none;

use crate::commands::validate_worker_name;
use crate::settings::toml::deploy_config::{DeployConfig, RouteConfig};
use crate::settings::toml::environment::Environment;
use crate::settings::toml::kv_namespace::KvNamespace;
use crate::settings::toml::site::Site;
use crate::settings::toml::target_type::TargetType;
use crate::settings::toml::Target;
use crate::terminal::emoji;
use crate::terminal::message;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Manifest {
    #[serde(default)]
    pub name: String,
    #[serde(rename = "type")]
    pub target_type: TargetType,
    #[serde(default)]
    pub account_id: String,
    pub workers_dev: Option<bool>,
    #[serde(default, with = "string_empty_as_none")]
    pub route: Option<String>,
    pub routes: Option<Vec<String>>,
    #[serde(default, with = "string_empty_as_none")]
    pub zone_id: Option<String>,
    pub webpack_config: Option<String>,
    pub private: Option<bool>,
    // TODO: maybe one day, serde toml support will allow us to serialize sites
    // as a TOML inline table (this would prevent confusion with environments too!)
    pub site: Option<Site>,
    #[serde(rename = "kv-namespaces")]
    pub kv_namespaces: Option<Vec<KvNamespace>>,
    pub env: Option<HashMap<String, Environment>>,
}

impl Manifest {
    pub fn new(config_path: &Path) -> Result<Self, failure::Error> {
        let config = read_config(config_path)?;

        let manifest: Manifest = match config.try_into() {
            Ok(m) => m,
            Err(e) => {
                if e.to_string().contains("unknown field `kv-namespaces`") {
                    failure::bail!("kv-namespaces should not live under the [site] table in wrangler.toml; please move it above [site].")
                } else {
                    failure::bail!(e)
                }
            }
        };

        check_for_duplicate_names(&manifest)?;

        Ok(manifest)
    }

    pub fn generate(
        name: String,
        target_type: Option<TargetType>,
        config_path: &PathBuf,
        site: Option<Site>,
    ) -> Result<Manifest, failure::Error> {
        let config_file = config_path.join("wrangler.toml");
        let template_config_content = fs::read_to_string(&config_file);
        let template_config = match &template_config_content {
            Ok(content) => {
                let config: Manifest = toml::from_str(content)?;
                config.warn_on_account_info();
                if let Some(target_type) = &target_type {
                    if config.target_type != *target_type {
                        message::warn(&format!("The template recommends the \"{}\" type. Using type \"{}\" may cause errors, we recommend changing the type field in wrangler.toml to \"{}\"", config.target_type, target_type, config.target_type));
                    }
                }
                Ok(config)
            }
            Err(err) => Err(err),
        };
        let mut template_config = match template_config {
            Ok(config) => config,
            Err(err) => {
                log::info!("Error parsing template {}", err);
                log::debug!("template content {:?}", template_config_content);
                Manifest::default()
            }
        };

        let default_workers_dev = match &template_config.route {
            Some(route) => {
                if route.is_empty() {
                    Some(true)
                } else {
                    None
                }
            }
            None => Some(true),
        };

        template_config.name = name;
        template_config.workers_dev = default_workers_dev;
        if let Some(target_type) = &target_type {
            template_config.target_type = target_type.clone();
        }

        if let Some(arg_site) = site {
            if template_config.site.is_none() {
                template_config.site = Some(arg_site);
            }
        }

        // TODO: https://github.com/cloudflare/wrangler/issues/773

        let toml = toml::to_string(&template_config)?;

        log::info!("Writing a wrangler.toml file at {}", config_file.display());
        fs::write(&config_file, &toml)?;
        Ok(template_config)
    }

    pub fn worker_name(&self, env_arg: Option<&str>) -> String {
        if let Some(environment) = self.get_environment(env_arg).unwrap_or_default() {
            if let Some(name) = &environment.name {
                return name.clone();
            }
            if let Some(env) = env_arg {
                return format!("{}-{}", self.name, env);
            }
        }

        self.name.clone()
    }

    fn route_config(&self) -> RouteConfig {
        RouteConfig {
            account_id: Some(self.account_id.clone()),
            workers_dev: self.workers_dev,
            route: self.route.clone(),
            routes: self.routes.clone(),
            zone_id: self.zone_id.clone(),
        }
    }

    pub fn deploy_config(&self, env: Option<&str>) -> Result<DeployConfig, failure::Error> {
        let script = self.worker_name(env);
        validate_worker_name(&script)?;

        if let Some(environment) = self.get_environment(env)? {
            // if there is an environment level deploy target, try to return that
            if let Some(env_route_config) =
                environment.route_config(self.account_id.clone(), self.zone_id.clone())
            {
                DeployConfig::build(&script, &env_route_config)
            } else {
                // If the top level config is Zoned, the user needs to specify new route config
                let top_level_config = DeployConfig::build(&script, &self.route_config())?;
                match top_level_config {
                    DeployConfig::Zoned(_) => failure::bail!(
                        "you must specify route(s) per environment for zoned deploys."
                    ),
                    DeployConfig::Zoneless(_) => Ok(top_level_config),
                }
            }
        } else {
            DeployConfig::build(&script, &self.route_config())
        }
    }

    pub fn get_target(&self, environment_name: Option<&str>) -> Result<Target, failure::Error> {
        // Site projects are always webpack for now; don't let toml override this.
        let target_type = match self.site {
            Some(_) => TargetType::Webpack,
            None => self.target_type.clone(),
        };

        let mut target = Target {
            target_type,                                 // MUST inherit
            account_id: self.account_id.clone(),         // MAY inherit
            webpack_config: self.webpack_config.clone(), // MAY inherit
            // importantly, the top level name will be modified
            // to include the name of the environment
            name: self.name.clone(),                   // MAY inherit
            kv_namespaces: self.kv_namespaces.clone(), // MUST NOT inherit
            site: self.site.clone(),                   // MUST NOT inherit
        };

        let environment = self.get_environment(environment_name)?;

        if let Some(environment) = environment {
            target.name = self.worker_name(environment_name);
            if let Some(account_id) = &environment.account_id {
                target.account_id = account_id.clone();
            }
            if environment.webpack_config.is_some() {
                target.webpack_config = environment.webpack_config.clone();
            }
            // don't inherit kv namespaces because it is an anti-pattern to use the same namespaces across multiple environments
            target.kv_namespaces = environment.kv_namespaces.clone();
        }

        Ok(target)
    }

    pub fn get_environment(
        &self,
        environment_name: Option<&str>,
    ) -> Result<Option<&Environment>, failure::Error> {
        // check for user-specified environment name
        if let Some(environment_name) = environment_name {
            if let Some(environment_table) = &self.env {
                if let Some(environment) = environment_table.get(environment_name) {
                    Ok(Some(environment))
                } else {
                    failure::bail!(format!(
                        "{} Could not find environment with name \"{}\"",
                        emoji::WARN,
                        environment_name
                    ))
                }
            } else {
                failure::bail!(format!(
                    "{} There are no environments specified in your wrangler.toml",
                    emoji::WARN
                ))
            }
        } else {
            Ok(None)
        }
    }

    fn warn_on_account_info(&self) {
        let account_id_env = env::var("CF_ACCOUNT_ID").is_ok();
        let zone_id_env = env::var("CF_ZONE_ID").is_ok();
        let mut top_level_fields: Vec<String> = Vec::new();
        if !account_id_env {
            top_level_fields.push("account_id".to_string());
        }
        if let Some(kv_namespaces) = &self.kv_namespaces {
            for kv_namespace in kv_namespaces {
                top_level_fields.push(format!(
                    "kv-namespace {} needs a namespace_id",
                    kv_namespace.binding
                ));
            }
        }
        if let Some(route) = &self.route {
            if !route.is_empty() {
                top_level_fields.push("route".to_string());
            }
        }
        if let Some(zone_id) = &self.zone_id {
            if !zone_id.is_empty() && !zone_id_env {
                top_level_fields.push("zone_id".to_string());
            }
        }

        let mut env_fields: HashMap<String, Vec<String>> = HashMap::new();

        if let Some(env) = &self.env {
            for (env_name, env) in env {
                let mut current_env_fields: Vec<String> = Vec::new();
                if env.account_id.is_some() && !account_id_env {
                    current_env_fields.push("account_id".to_string());
                }
                if let Some(kv_namespaces) = &env.kv_namespaces {
                    for kv_namespace in kv_namespaces {
                        current_env_fields.push(format!(
                            "kv-namespace {} needs a namespace_id",
                            kv_namespace.binding
                        ));
                    }
                }
                if let Some(route) = &env.route {
                    if !route.is_empty() {
                        current_env_fields.push("route".to_string());
                    }
                }
                if let Some(zone_id) = &env.zone_id {
                    if !zone_id.is_empty() && !zone_id_env {
                        current_env_fields.push("zone_id".to_string());
                    }
                }
                if !current_env_fields.is_empty() {
                    env_fields.insert(env_name.to_string(), current_env_fields);
                }
            }
        }
        let has_top_level_fields = !top_level_fields.is_empty();
        let has_env_fields = !env_fields.is_empty();
        let mut needs_new_line = false;
        if has_top_level_fields || has_env_fields {
            message::help(
                "You will need to update the following fields in the created wrangler.toml file before continuing:"
            );
            message::help(
                "You can find your account_id and zone_id in the right sidebar of the zone overview tab at https://dash.cloudflare.com"
            );
            if has_top_level_fields {
                needs_new_line = true;
                for top_level_field in top_level_fields {
                    println!("- {}", top_level_field);
                }
            }
            if has_env_fields {
                for (env_name, env_fields) in env_fields {
                    if needs_new_line {
                        println!();
                    }
                    println!("[env.{}]", env_name);
                    needs_new_line = true;
                    for env_field in env_fields {
                        println!("  - {}", env_field);
                    }
                }
            }
        }
    }
}

impl FromStr for Manifest {
    type Err = toml::de::Error;

    fn from_str(serialized_toml: &str) -> Result<Self, Self::Err> {
        toml::from_str(serialized_toml)
    }
}

fn read_config(config_path: &Path) -> Result<Config, failure::Error> {
    let mut config = Config::new();

    let config_str = config_path
        .to_str()
        .expect("project config path should be a string");
    config.merge(File::with_name(config_str))?;

    // Eg.. `CF_ACCOUNT_AUTH_KEY=farts` would set the `account_auth_key` key
    config.merge(config::Environment::with_prefix("CF"))?;

    Ok(config)
}

fn check_for_duplicate_names(manifest: &Manifest) -> Result<(), failure::Error> {
    let mut names: HashSet<String> = HashSet::new();
    let mut duplicate_names: HashSet<String> = HashSet::new();
    names.insert(manifest.name.to_string());
    if let Some(environments) = &manifest.env {
        for (_, environment) in environments.iter() {
            if let Some(name) = &environment.name {
                if names.contains(name) && !duplicate_names.contains(name) {
                    duplicate_names.insert(name.to_string());
                } else {
                    names.insert(name.to_string());
                }
            }
        }
    }
    let duplicate_name_string = duplicate_names
        .clone()
        .into_iter()
        .collect::<Vec<String>>()
        .join(", ");
    let duplicate_message = match duplicate_names.len() {
        1 => Some("this name is duplicated".to_string()),
        n if n >= 2 => Some("these names are duplicated".to_string()),
        _ => None,
    };
    if let Some(message) = duplicate_message {
        failure::bail!(format!(
            "{} Each name in your `wrangler.toml` must be unique, {}: {}",
            emoji::WARN,
            message,
            duplicate_name_string
        ))
    }
    Ok(())
}
