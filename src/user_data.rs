use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::Api;
use minijinja::{Environment, UndefinedBehavior, Value};
use serde::Serialize;
use tonic::Status;

use crate::crd::{TemplateVariable, TemplateVariableSource};

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("template variable '{0}' collides with built-in scope (node/asg)")]
    ReservedName(String),
    #[error("template variable '{0}' has neither value nor valueFrom")]
    MissingSource(String),
    #[error("template variable '{0}' sets both value and valueFrom")]
    AmbiguousSource(String),
    #[error("template variable '{0}' valueFrom sets both secretKeyRef and configMapKeyRef")]
    AmbiguousValueFrom(String),
    #[error("template variable '{0}' valueFrom sets neither secretKeyRef nor configMapKeyRef")]
    EmptyValueFrom(String),
    #[error("secret {ns}/{name} not found")]
    SecretNotFound { ns: String, name: String },
    #[error("secret {ns}/{name} missing key '{key}'")]
    SecretMissingKey {
        ns: String,
        name: String,
        key: String,
    },
    #[error("secret {ns}/{name} key '{key}' is not valid UTF-8")]
    SecretNotUtf8 {
        ns: String,
        name: String,
        key: String,
    },
    #[error("configmap {ns}/{name} not found")]
    ConfigMapNotFound { ns: String, name: String },
    #[error("configmap {ns}/{name} missing key '{key}'")]
    ConfigMapMissingKey {
        ns: String,
        name: String,
        key: String,
    },
    #[error("kube API error: {0}")]
    Kube(#[from] kube::Error),
    #[error("template error: {0}")]
    Template(String),
}

impl From<RenderError> for Status {
    fn from(e: RenderError) -> Self {
        use RenderError::*;
        match &e {
            ReservedName(_)
            | AmbiguousSource(_)
            | MissingSource(_)
            | AmbiguousValueFrom(_)
            | EmptyValueFrom(_) => Status::invalid_argument(e.to_string()),
            SecretNotFound { .. } | ConfigMapNotFound { .. } => Status::not_found(e.to_string()),
            SecretMissingKey { .. } | ConfigMapMissingKey { .. } | SecretNotUtf8 { .. } => {
                Status::failed_precondition(e.to_string())
            }
            Template(_) => Status::failed_precondition(e.to_string()),
            Kube(_) => Status::internal(e.to_string()),
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct NodeVars {
    pub name: String,
    pub hostname: String,
    pub index: i32,
    pub password: String,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AsgVars {
    pub name: String,
    pub size: String,
    pub region: String,
    pub image: String,
    pub name_prefix: String,
    pub vcpus: Option<i32>,
    pub memory_mb: Option<i32>,
    pub disk_gb: Option<i32>,
}

pub struct BuiltinVars {
    pub node: NodeVars,
    pub asg: AsgVars,
}

/// Validate variable names without touching the cluster. Run this before any
/// API call so we fail fast on user errors.
fn validate_variables(vars: &[TemplateVariable]) -> Result<(), RenderError> {
    for v in vars {
        if v.name == "node" || v.name == "asg" {
            return Err(RenderError::ReservedName(v.name.clone()));
        }
        match (&v.value, &v.value_from) {
            (Some(_), Some(_)) => return Err(RenderError::AmbiguousSource(v.name.clone())),
            (None, None) => return Err(RenderError::MissingSource(v.name.clone())),
            (None, Some(src)) => match (&src.secret_key_ref, &src.config_map_key_ref) {
                (Some(_), Some(_)) => {
                    return Err(RenderError::AmbiguousValueFrom(v.name.clone()));
                }
                (None, None) => return Err(RenderError::EmptyValueFrom(v.name.clone())),
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

/// Resolve every `TemplateVariable` to a concrete string, reading Secrets and
/// ConfigMaps as needed. Called once per scale-up; values are static for all
/// nodes created in the same increase-size RPC.
pub async fn resolve_variables(
    k8s: &kube::Client,
    default_ns: &str,
    vars: &[TemplateVariable],
) -> Result<BTreeMap<String, String>, RenderError> {
    validate_variables(vars)?;
    let mut out = BTreeMap::new();
    for v in vars {
        let value = if let Some(lit) = &v.value {
            lit.clone()
        } else {
            // Safe: validate_variables ensured exactly one of secretKeyRef /
            // configMapKeyRef is set.
            resolve_source(k8s, default_ns, v.value_from.as_ref().unwrap()).await?
        };
        out.insert(v.name.clone(), value);
    }
    Ok(out)
}

async fn resolve_source(
    k8s: &kube::Client,
    default_ns: &str,
    src: &TemplateVariableSource,
) -> Result<String, RenderError> {
    if let Some(sel) = &src.secret_key_ref {
        let ns = sel.namespace.as_deref().unwrap_or(default_ns);
        let api: Api<Secret> = Api::namespaced(k8s.clone(), ns);
        let secret = api
            .get_opt(&sel.name)
            .await?
            .ok_or_else(|| RenderError::SecretNotFound {
                ns: ns.to_string(),
                name: sel.name.clone(),
            })?;
        let data = secret.data.ok_or_else(|| RenderError::SecretMissingKey {
            ns: ns.to_string(),
            name: sel.name.clone(),
            key: sel.key.clone(),
        })?;
        let bytes = data
            .get(&sel.key)
            .ok_or_else(|| RenderError::SecretMissingKey {
                ns: ns.to_string(),
                name: sel.name.clone(),
                key: sel.key.clone(),
            })?;
        return String::from_utf8(bytes.0.clone()).map_err(|_| RenderError::SecretNotUtf8 {
            ns: ns.to_string(),
            name: sel.name.clone(),
            key: sel.key.clone(),
        });
    }
    let sel = src
        .config_map_key_ref
        .as_ref()
        .expect("validate_variables guarantees configMapKeyRef is set when secretKeyRef is absent");
    let ns = sel.namespace.as_deref().unwrap_or(default_ns);
    let api: Api<ConfigMap> = Api::namespaced(k8s.clone(), ns);
    let cm = api
        .get_opt(&sel.name)
        .await?
        .ok_or_else(|| RenderError::ConfigMapNotFound {
            ns: ns.to_string(),
            name: sel.name.clone(),
        })?;
    let data = cm.data.ok_or_else(|| RenderError::ConfigMapMissingKey {
        ns: ns.to_string(),
        name: sel.name.clone(),
        key: sel.key.clone(),
    })?;
    data.get(&sel.key)
        .cloned()
        .ok_or_else(|| RenderError::ConfigMapMissingKey {
            ns: ns.to_string(),
            name: sel.name.clone(),
            key: sel.key.clone(),
        })
}

pub fn render(
    template: &str,
    builtins: &BuiltinVars,
    user_vars: &BTreeMap<String, String>,
) -> Result<String, RenderError> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    let mut ctx: BTreeMap<String, Value> = BTreeMap::new();
    ctx.insert("node".into(), Value::from_serialize(&builtins.node));
    ctx.insert("asg".into(), Value::from_serialize(&builtins.asg));
    for (k, v) in user_vars {
        if k == "node" || k == "asg" {
            return Err(RenderError::ReservedName(k.clone()));
        }
        ctx.insert(k.clone(), Value::from(v.clone()));
    }
    env.render_str(template, ctx)
        .map_err(|e| RenderError::Template(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{KeySelector, TemplateVariable, TemplateVariableSource};

    fn sample_builtins() -> BuiltinVars {
        BuiltinVars {
            node: NodeVars {
                name: "worker-1".into(),
                hostname: "worker-1".into(),
                index: 0,
                password: "supersecret".into(),
            },
            asg: AsgVars {
                name: "workers".into(),
                size: "std-2".into(),
                region: "syd".into(),
                image: "ubuntu-22.04".into(),
                name_prefix: "".into(),
                vcpus: Some(2),
                memory_mb: Some(4096),
                disk_gb: Some(40),
            },
        }
    }

    #[test]
    fn render_builtins() {
        let out = render(
            "name={{ node.name }} region={{ asg.region }} mem={{ asg.memoryMb }}",
            &sample_builtins(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(out, "name=worker-1 region=syd mem=4096");
    }

    #[test]
    fn render_user_var() {
        let mut vars = BTreeMap::new();
        vars.insert("joinToken".to_string(), "abc.def".to_string());
        let out = render(
            "{{ joinToken }} on {{ node.name }}",
            &sample_builtins(),
            &vars,
        )
        .unwrap();
        assert_eq!(out, "abc.def on worker-1");
    }

    #[test]
    fn undefined_is_strict() {
        let err = render("{{ mystery }}", &sample_builtins(), &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, RenderError::Template(_)));
    }

    #[test]
    fn user_var_cannot_shadow_scopes() {
        let mut vars = BTreeMap::new();
        vars.insert("node".to_string(), "bad".to_string());
        let err = render("{{ node }}", &sample_builtins(), &vars).unwrap_err();
        assert!(matches!(err, RenderError::ReservedName(_)));
    }

    #[test]
    fn syntax_error_surfaces() {
        let err = render("{{ unclosed", &sample_builtins(), &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, RenderError::Template(_)));
    }

    #[test]
    fn validate_literal_ok() {
        let vars = vec![TemplateVariable {
            name: "foo".into(),
            value: Some("bar".into()),
            value_from: None,
        }];
        validate_variables(&vars).unwrap();
    }

    #[test]
    fn validate_reserved_name() {
        let vars = vec![TemplateVariable {
            name: "node".into(),
            value: Some("x".into()),
            value_from: None,
        }];
        assert!(matches!(
            validate_variables(&vars),
            Err(RenderError::ReservedName(_))
        ));
    }

    #[test]
    fn validate_missing_source() {
        let vars = vec![TemplateVariable {
            name: "foo".into(),
            value: None,
            value_from: None,
        }];
        assert!(matches!(
            validate_variables(&vars),
            Err(RenderError::MissingSource(_))
        ));
    }

    #[test]
    fn validate_ambiguous_source() {
        let vars = vec![TemplateVariable {
            name: "joinToken".into(),
            value: Some("lit".into()),
            value_from: Some(TemplateVariableSource {
                secret_key_ref: Some(KeySelector {
                    name: "s".into(),
                    namespace: None,
                    key: "k".into(),
                }),
                config_map_key_ref: None,
            }),
        }];
        assert!(matches!(
            validate_variables(&vars),
            Err(RenderError::AmbiguousSource(_))
        ));
    }

    #[test]
    fn validate_ambiguous_value_from() {
        let vars = vec![TemplateVariable {
            name: "foo".into(),
            value: None,
            value_from: Some(TemplateVariableSource {
                secret_key_ref: Some(KeySelector {
                    name: "s".into(),
                    namespace: None,
                    key: "k".into(),
                }),
                config_map_key_ref: Some(KeySelector {
                    name: "c".into(),
                    namespace: None,
                    key: "k".into(),
                }),
            }),
        }];
        assert!(matches!(
            validate_variables(&vars),
            Err(RenderError::AmbiguousValueFrom(_))
        ));
    }

    #[test]
    fn validate_empty_value_from() {
        let vars = vec![TemplateVariable {
            name: "foo".into(),
            value: None,
            value_from: Some(TemplateVariableSource::default()),
        }];
        assert!(matches!(
            validate_variables(&vars),
            Err(RenderError::EmptyValueFrom(_))
        ));
    }
}
