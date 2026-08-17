/// Spreading activation: propagate heat along graph edges.
///
/// When a memory is accessed, a fraction of heat passes to its neighbors,
/// diminishing per hop. **Heat only, not stability** — second-hand
/// activation warms temporarily but doesn't make memories more durable.

/// A neighbor that should receive propagated heat.
#[derive(Debug, Clone)]
pub struct ActivationTarget {
    /// The record ID of the neighbor to warm.
    pub id: String,
    /// The heat to add to this neighbor.
    pub heat_delta: f32,
    /// How many hops away from the source.
    pub hop: u32,
}

/// Configuration for spreading activation.
#[derive(Debug, Clone)]
pub struct ActivationConfig {
    /// Fraction of heat passed per hop. Default 0.3.
    pub propagation_factor: f32,
    /// Max graph hops for activation. Default 2.
    pub max_hops: u32,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            propagation_factor: 0.3,
            max_hops: 2,
        }
    }
}

/// Compute activation targets from a set of neighbors with hop distances.
///
/// Takes the direct heat bump applied to the accessed memory, and computes
/// how much heat each neighbor should receive based on its distance.
///
/// This is pure computation — the caller is responsible for actually writing
/// the heat updates to the database.
pub fn compute_activation_targets(
    neighbors: &[(String, u32, f64)], // (id, hop, edge_strength)
    direct_bump: f32,
    config: &ActivationConfig,
) -> Vec<ActivationTarget> {
    neighbors
        .iter()
        .filter(|(_, hop, _)| *hop <= config.max_hops)
        .map(|(id, hop, strength)| {
            // Heat decays exponentially with hops
            let hop_decay = config.propagation_factor.powi(*hop as i32);
            // Edge strength modulates the propagation
            let heat_delta = direct_bump * hop_decay * (*strength as f32);
            ActivationTarget {
                id: id.clone(),
                heat_delta,
                hop: *hop,
            }
        })
        .filter(|t| t.heat_delta > 0.001) // Skip negligible activations
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_hop_activation() {
        let config = ActivationConfig {
            propagation_factor: 0.3,
            max_hops: 2,
        };
        let neighbors = vec![("fact:b".to_string(), 1, 1.0)];
        let targets = compute_activation_targets(&neighbors, 1.0, &config);
        assert_eq!(targets.len(), 1);
        assert!((targets[0].heat_delta - 0.3).abs() < 0.001);
        assert_eq!(targets[0].hop, 1);
    }

    #[test]
    fn test_two_hop_activation() {
        let config = ActivationConfig {
            propagation_factor: 0.3,
            max_hops: 2,
        };
        let neighbors = vec![
            ("fact:b".to_string(), 1, 1.0),
            ("fact:c".to_string(), 2, 1.0),
        ];
        let targets = compute_activation_targets(&neighbors, 1.0, &config);
        assert_eq!(targets.len(), 2);

        let hop1 = targets.iter().find(|t| t.hop == 1).unwrap();
        let hop2 = targets.iter().find(|t| t.hop == 2).unwrap();

        // Hop 1: 1.0 * 0.3^1 = 0.3
        assert!((hop1.heat_delta - 0.3).abs() < 0.001);
        // Hop 2: 1.0 * 0.3^2 = 0.09
        assert!((hop2.heat_delta - 0.09).abs() < 0.001);
    }

    #[test]
    fn test_beyond_max_hops_filtered() {
        let config = ActivationConfig {
            propagation_factor: 0.3,
            max_hops: 1,
        };
        let neighbors = vec![
            ("fact:b".to_string(), 1, 1.0),
            ("fact:c".to_string(), 2, 1.0), // beyond max_hops
        ];
        let targets = compute_activation_targets(&neighbors, 1.0, &config);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "fact:b");
    }

    #[test]
    fn test_edge_strength_modulates() {
        let config = ActivationConfig {
            propagation_factor: 0.3,
            max_hops: 2,
        };
        let neighbors = vec![
            ("fact:b".to_string(), 1, 0.5), // half strength
        ];
        let targets = compute_activation_targets(&neighbors, 1.0, &config);
        // 1.0 * 0.3 * 0.5 = 0.15
        assert!((targets[0].heat_delta - 0.15).abs() < 0.001);
    }

    #[test]
    fn test_negligible_activation_filtered() {
        let config = ActivationConfig {
            propagation_factor: 0.1,
            max_hops: 3,
        };
        let neighbors = vec![
            ("fact:b".to_string(), 3, 0.05), // 1.0 * 0.1^3 * 0.05 = 0.00005
        ];
        let targets = compute_activation_targets(&neighbors, 1.0, &config);
        assert!(targets.is_empty()); // Filtered as negligible
    }
}
