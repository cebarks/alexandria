pub struct Config {
    pub embedding_model: String,
    pub embedding_device: String,
    pub cluster_join_threshold: f32,
    pub heat_spacing_halflife: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            embedding_model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            embedding_device: "cpu".to_string(),
            cluster_join_threshold: 0.75,
            heat_spacing_halflife: 86400.0,
        }
    }
}
