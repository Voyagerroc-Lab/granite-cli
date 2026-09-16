mod secret;
pub use secret::Secret;

// TODO: There are a number of instances of `#[allow(unused)]` that allow
// certain portions of the factories to be unused without warning. These should
// be removed once all factories are populated and utilized.

/*-- Generic Factory Infrastructure ------------------------------------------*/

/// Unit struct for types that have no structured config.
/// Used by test doubles and impls that genuinely have no config to declare.
#[derive(schemars::JsonSchema, serde::Serialize, serde::Deserialize, Default)]
pub struct NoConfig {}

/// Core trait that all factory-managed types must implement.
/// Provides construction from a configuration object.
pub trait ConfigConstructable {
    /// The structured config type for this implementation.
    /// Must implement `JsonSchema + Serialize + Default`.
    type Config: schemars::JsonSchema + serde::Serialize + Default;

    /// Construct with the instance's configured name, a config instance, and
    /// the global application config.
    ///
    /// `instance_id` is the key this instance is configured under (e.g. the
    /// provider nickname `my-ollama`), *not* the registry type name. Implementations
    /// that need to identify themselves downstream store it and surface it via
    /// [`Named`]. For instances constructed outside any configured set (bare
    /// catalog lookups, `--output` backends), callers pass the type name.
    ///
    /// Most implementations ignore `global_config`; types that need cross-registry
    /// resolution (e.g. resolving a model's provider) use it.
    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self
    where
        Self: Sized;
}

/// Lets a factory-constructed instance report the configured name it was built
/// for. Object-safe on purpose: the domain traits (`Provider`, `Model`,
/// `Capability`, `Launcher`) require it, so a `&dyn Provider` can answer "which
/// configured provider are you?" without downcasting.
///
/// The value is whatever `instance_id` was passed to
/// [`ConfigConstructable::new`], so it round-trips the config key that produced
/// the instance.
pub trait Named {
    fn instance_id(&self) -> &str;
}

/// Macro to define a complete factory infrastructure for a trait hierarchy.
///
/// This macro generates:
/// - An internal metadata trait for type erasure
/// - A HasMetadata trait that implementations must provide
/// - A MetaOf wrapper for connecting implementations to metadata
/// - A Factory struct with registration and construction capabilities
///
/// # Arguments
///
/// * `$trait` - The trait being factored (e.g., Provider, Capability)
/// * `$config` - The config type used for construction
/// * `$metadata` - The metadata type returned by describe()
/// * `$factory` - Name for the Factory struct
///
/// # Example
///
/// ```ignore
/// trait MyTrait: ConfigConstructable {
///     fn do_something(&self);
/// }
/// struct MyMetadata { value: i32 }
///
/// define_factory!(
///     MyTrait,
///     MyMetadata,
///     MyTraitFactory
/// );
///
/// struct MySomething { value: i32 }
/// impl MyTrait for MySomething {
///     fn do_something(&self) { println!("My value: {}", self.value); }
/// }
/// impl HasMyTraitMetadata for MySomething {
///     fn metadata() -> String { "I belong to you".to_string() }
/// }
/// ```
#[macro_export]
macro_rules! define_factory {
    ($trait:ident, $metadata:ty, $factory:ident) => {
        /// Wrapper type that connects an implementation to its metadata.
        /// Uses PhantomData to maintain type information without storing instances.
        struct MetaOf<T>(std::marker::PhantomData<T>);
        impl<T> MetaOf<T> {
            #[allow(unused)]
            const fn new() -> Self {
                Self(std::marker::PhantomData)
            }
        }

        $crate::paste::paste! {
            /// Internal trait for metadata provision and construction.
            /// This trait enables type erasure while maintaining type safety.
            pub(crate) trait [<$trait Metadata_>]: Send + Sync {
                /// Get metadata describing this implementation
                fn describe(&self) -> $metadata;

                /// Construct an instance with its configured name, config, and global application config
                #[allow(unused)]
                fn construct(&self, instance_id: &str, cfg: &serde_json::Value, global_config: &$crate::config::Config) -> Box<dyn $trait>;

                /// JSON schema of the config this implementation expects
                #[allow(unused)]
                fn config_schema(&self) -> schemars::Schema;

                /// Default config value for this implementation
                #[allow(unused)]
                fn default_config(&self) -> serde_json::Value;
            }

            /// Trait that implementations must provide to supply metadata.
            /// This is the public interface for implementations to describe themselves.
            pub trait [<Has $trait Metadata>] {
                /// Return metadata describing this implementation
                fn metadata() -> $metadata;
            }

            /// Implementation of the internal metadata trait for any type T
            /// that implements the required traits.
            impl<T> [<$trait Metadata_>] for MetaOf<T>
            where
                T: $trait
                    + [<Has $trait Metadata>]
                    + ConfigConstructable<Config: schemars::JsonSchema + serde::Serialize + Default>
                    + Send
                    + Sync
                    + 'static,
            {
                fn describe(&self) -> $metadata {
                    T::metadata()
                }

                fn construct(&self, instance_id: &str, cfg: &serde_json::Value, global_config: &$crate::config::Config) -> Box<dyn $trait> {
                    Box::new(T::new(instance_id, cfg, global_config))
                }

                fn config_schema(&self) -> schemars::Schema {
                    schemars::schema_for!(<T as ConfigConstructable>::Config)
                }

                fn default_config(&self) -> serde_json::Value {
                    serde_json::to_value(<T as ConfigConstructable>::Config::default())
                        .unwrap_or_default()
                }
            }

            /// Factory for creating and managing instances of the trait.
            ///
            /// The factory maintains a registry of implementations and provides
            /// methods to:
            /// - Register new implementations
            /// - Construct instances by name
            /// - Query metadata
            /// - List all registered implementations
            pub struct $factory {
                registry: std::collections::HashMap<&'static str, Box<dyn [<$trait Metadata_>]>>,
            }

            impl $factory {
                /// Create a new empty factory
                pub(crate) fn new() -> Self {
                    Self {
                        registry: std::collections::HashMap::new(),
                    }
                }

                /// Register an implementation with the given name.
                ///
                /// # Type Parameters
                ///
                /// * `T` - The implementation type to register
                ///
                /// # Arguments
                ///
                /// * `name` - Static string identifier for this implementation
                #[allow(unused)]
                pub(crate) fn register<T>(&mut self, name: &'static str)
                where
                    T: $trait
                        + ConfigConstructable<
                            Config: schemars::JsonSchema + serde::Serialize + Default,
                        > + [<Has $trait Metadata>]
                        + Send
                        + Sync
                        + 'static,
                {
                    self.registry.insert(name, Box::new(MetaOf::<T>::new()));
                }

                /// Construct an instance by name with the given configuration.
                ///
                /// `name` selects the registered implementation. `instance_id`,
                /// `cfg` and `global_config` go to that implementation's
                /// `ConfigConstructable::new` unchanged, so `cfg` is what decides
                /// the instance:
                /// - A saved instance's config produces that configured instance,
                ///   and `instance_id` is the config key it came from.
                /// - A default or ad-hoc config produces an ephemeral instance of
                ///   a type that is not configured, built to inspect it. The API
                ///   expects `name` as the value of `instance_id` in this case,
                ///   since there is no config key.
                ///
                /// `instance_id` is only the label the instance reports through
                /// [`Named`]. It is never looked up in `global_config`, so an id
                /// that is not configured constructs the same as one that is.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation to construct
                /// * `instance_id` - The configured name of this instance (the config
                ///   key it came from). Pass `name` for instances that aren't drawn
                ///   from a configured set.
                /// * `cfg` - Configuration to pass to the constructor
                ///
                /// # Returns
                ///
                /// * `Ok(Box<dyn Trait>)` - Successfully constructed instance
                /// * `Err(String)` - Error message if name not found
                #[allow(unused)]
                pub(crate) fn construct(
                    &self,
                    name: &str,
                    instance_id: &str,
                    cfg: &serde_json::Value,
                    global_config: &$crate::config::Config,
                ) -> Result<Box<dyn $trait>, String> {
                    self.registry
                        .get(name)
                        .map(|x| x.construct(instance_id, cfg, global_config))
                        .ok_or_else(|| format!("Unknown instance type: {}", name))
                }

                /// Get metadata for a specific implementation by name.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation
                ///
                /// # Returns
                ///
                /// * `Some(metadata)` - Metadata if found
                /// * `None` - If name not registered
                #[allow(unused)]
                pub(crate) fn get(&self, name: &str) -> Option<$metadata> {
                    self.registry.get(name).map(|x| x.describe())
                }

                /// Get all registered implementations with their metadata.
                ///
                /// # Returns
                ///
                /// HashMap mapping names to metadata for all registered implementations
                #[allow(unused)]
                pub(crate) fn entries(&self) -> std::collections::HashMap<&str, $metadata> {
                    self.registry
                        .iter()
                        .map(|(k, v)| (*k, v.describe()))
                        .collect()
                }

                /// Get the config JSON schema for a specific implementation by name.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation
                ///
                /// # Returns
                ///
                /// * `Some(schema)` - Schema of the config `construct` expects, if found
                /// * `None` - If name not registered
                #[allow(unused)]
                pub(crate) fn config_schema(&self, name: &str) -> Option<schemars::Schema> {
                    self.registry.get(name).map(|x| x.config_schema())
                }

                /// Get the default config value for a specific implementation by name.
                ///
                /// # Arguments
                ///
                /// * `name` - The name of the implementation
                ///
                /// # Returns
                ///
                /// * `Some(value)` - Default config value, if found
                /// * `None` - If name not registered
                #[allow(unused)]
                pub(crate) fn default_config(&self, name: &str) -> Option<serde_json::Value> {
                    self.registry.get(name).map(|x| x.default_config())
                }
            }
        }

        impl Default for $factory {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $crate::dependency::Catalogued for dyn $trait {
            type Metadata = $metadata;
        }
    };
}

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    // Hoist paste macro for use in the macro-expanded traits
    extern crate paste;

    // Test trait and types
    pub(crate) trait TestTrait: Named {
        fn get_value(&self) -> i32;
    }

    // Define factory for test trait (3 params: trait, metadata type, factory name)
    define_factory!(TestTrait, String, TestTraitFactory);

    // Test implementation 1
    struct TestImpl1 {
        instance_id: String,
        value: i32,
    }

    impl ConfigConstructable for TestImpl1 {
        type Config = NoConfig;

        fn new(
            instance_id: &str,
            cfg: &serde_json::Value,
            _global_config: &crate::config::Config,
        ) -> Self {
            let value = cfg.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            Self {
                instance_id: instance_id.to_string(),
                value,
            }
        }
    }

    impl TestTrait for TestImpl1 {
        fn get_value(&self) -> i32 {
            self.value
        }
    }

    impl Named for TestImpl1 {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    impl HasTestTraitMetadata for TestImpl1 {
        fn metadata() -> String {
            "TestImpl1: A test implementation".to_string()
        }
    }

    // Test implementation 2
    struct TestImpl2 {
        instance_id: String,
        value: i32,
    }

    impl ConfigConstructable for TestImpl2 {
        type Config = TestImpl2Config;

        fn new(
            instance_id: &str,
            cfg: &serde_json::Value,
            _global_config: &crate::config::Config,
        ) -> Self {
            let value = cfg.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            Self {
                instance_id: instance_id.to_string(),
                value: value * 2,
            }
        }
    }

    impl TestTrait for TestImpl2 {
        fn get_value(&self) -> i32 {
            self.value
        }
    }

    impl Named for TestImpl2 {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[derive(schemars::JsonSchema, serde::Serialize, Default)]
    struct TestImpl2Config {
        #[allow(unused)] // Used for schema inspection
        value: i32,
    }

    impl HasTestTraitMetadata for TestImpl2 {
        fn metadata() -> String {
            "TestImpl2: Another test implementation".to_string()
        }
    }

    #[test]
    fn test_factory_registration() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        assert!(factory.get("impl1").is_some());
        assert!(factory.get("impl2").is_some());
        assert!(factory.get("impl3").is_none());
    }

    #[test]
    fn test_factory_metadata() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        let meta1 = factory.get("impl1").unwrap();
        assert!(meta1.contains("TestImpl1"));

        let meta2 = factory.get("impl2").unwrap();
        assert!(meta2.contains("TestImpl2"));
    }

    #[test]
    fn test_factory_construction() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        let cfg = serde_json::json!({ "value": 42 });
        let global_config = crate::config::Config::default();

        let inst1 = factory
            .construct("impl1", "my-impl1", &cfg, &global_config)
            .unwrap();
        assert_eq!(inst1.get_value(), 42);

        let inst2 = factory
            .construct("impl2", "my-impl2", &cfg, &global_config)
            .unwrap();
        assert_eq!(inst2.get_value(), 84); // TestImpl2 doubles the value
    }

    #[test]
    fn test_factory_construct_unknown() {
        let factory = TestTraitFactory::new();
        let cfg = serde_json::json!({ "value": 42 });
        let global_config = crate::config::Config::default();

        let result = factory.construct("unknown", "unknown", &cfg, &global_config);
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("Unknown instance type"));
    }

    #[test]
    fn test_factory_entries() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");
        factory.register::<TestImpl2>("impl2");

        let entries = factory.entries();
        assert_eq!(entries.len(), 2);
        let metadata_strs: Vec<String> = entries.into_values().collect();
        assert!(metadata_strs.iter().any(|s| s.contains("TestImpl1")));
        assert!(metadata_strs.iter().any(|s| s.contains("TestImpl2")));
    }

    #[test]
    fn test_factory_default() {
        let factory = TestTraitFactory::default();
        assert_eq!(factory.entries().len(), 0);
    }

    #[test]
    fn test_config_schema_default_is_opaque() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");

        // TestImpl1 uses NoConfig, which produces a proper schema for an empty object type.
        let schema = factory.config_schema("impl1").unwrap();
        assert_eq!(schema.get("type").and_then(|t| t.as_str()), Some("object"));
        assert_eq!(
            schema.get("title").and_then(|t| t.as_str()),
            Some("NoConfig")
        );
    }

    #[test]
    fn test_config_schema_uses_override() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl2>("impl2");

        let schema = factory.config_schema("impl2").unwrap();
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("object schema with properties");
        assert!(properties.contains_key("value"));
    }

    #[test]
    fn test_config_schema_unknown() {
        let factory = TestTraitFactory::new();
        assert!(factory.config_schema("unknown").is_none());
    }

    #[test]
    fn test_default_config_default_is_empty_object() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl1>("impl1");

        // TestImpl1 uses NoConfig, which serializes to an empty object.
        let value = factory.default_config("impl1").unwrap();
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn test_default_config_uses_override() {
        let mut factory = TestTraitFactory::new();
        factory.register::<TestImpl2>("impl2");

        let value = factory.default_config("impl2").unwrap();
        assert_eq!(value, serde_json::json!({ "value": 0 }));
    }

    #[test]
    fn test_default_config_unknown() {
        let factory = TestTraitFactory::new();
        assert!(factory.default_config("unknown").is_none());
    }
}
