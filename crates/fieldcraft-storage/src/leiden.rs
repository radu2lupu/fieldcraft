const EPSILON: f32 = 1.0e-6;

/// Partition a small weighted undirected graph into well-connected
/// communities maximizing CPM quality at the given resolution.
/// Deterministic: stable iteration order, index-based tie-breaking.
/// Nodes are `0..n`; edges are `(u, v, weight)` with `u < v`.
pub fn leiden_cpm(n: usize, edges: &[(usize, usize, f32)], resolution: f32) -> Vec<Vec<usize>> {
    if n == 0 {
        return Vec::new();
    }

    let mut weights = vec![vec![0.0_f32; n]; n];
    for &(u, v, weight) in edges {
        if u >= n || v >= n || u == v || weight <= 0.0 {
            continue;
        }
        weights[u][v] += weight;
        weights[v][u] += weight;
    }

    let mut node_sizes = vec![1_usize; n];
    let mut communities: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();

    for _ in 0..10 {
        let m = communities.len();
        let allowed: Vec<usize> = (0..m).collect();
        let labels: Vec<usize> = (0..m).collect();
        let moved = local_move(m, &weights, &node_sizes, labels, &allowed, resolution);
        let refined = refine(m, &weights, &node_sizes, &moved, resolution);
        let groups = communities_from_labels(&canonical_labels(m, &refined));
        if is_singleton_partition(&groups) {
            break;
        }

        let old_communities = communities;
        communities = groups
            .iter()
            .map(|group| {
                let mut members: Vec<usize> = group
                    .iter()
                    .flat_map(|&index| old_communities[index].iter().copied())
                    .collect();
                members.sort_unstable();
                members
            })
            .collect();
        node_sizes = groups
            .iter()
            .map(|group| group.iter().map(|&index| node_sizes[index]).sum())
            .collect();
        weights = aggregate_weights(&weights, &groups);
    }

    communities.sort_by_key(|community| community[0]);
    communities
}

fn local_move(
    n: usize,
    weights: &[Vec<f32>],
    node_sizes: &[usize],
    mut labels: Vec<usize>,
    allowed: &[usize],
    resolution: f32,
) -> Vec<usize> {
    let mut allowed_mask = vec![false; n];
    for &node in allowed {
        allowed_mask[node] = true;
    }

    let mut sizes = vec![0_usize; n];
    for &node in allowed {
        sizes[labels[node]] += node_sizes[node];
    }

    for _ in 0..100 {
        let mut moved = false;
        for &node in allowed {
            let old = labels[node];
            let old_size = sizes[old];
            let old_weight = weight_to_community(node, old, weights, &labels, allowed);
            let node_size = node_sizes[node];

            let mut candidates = Vec::new();
            let mut seen = vec![false; n];
            for other in 0..n {
                if !allowed_mask[other] || weights[node][other] <= 0.0 {
                    continue;
                }
                let target = labels[other];
                if target != old && !seen[target] {
                    seen[target] = true;
                    candidates.push(target);
                }
            }

            let mut best = old;
            let mut best_gain = 0.0_f32;
            for target in candidates {
                let target_weight = weight_to_community(node, target, weights, &labels, allowed);
                let gain =
                    target_weight - resolution * (node_size * sizes[target]) as f32 - old_weight
                        + resolution * (node_size * old_size.saturating_sub(node_size)) as f32;
                if gain > best_gain + EPSILON
                    || ((gain - best_gain).abs() <= EPSILON && gain > EPSILON && target < best)
                {
                    best = target;
                    best_gain = gain;
                }
            }

            if best != old {
                sizes[old] -= node_size;
                sizes[best] += node_size;
                labels[node] = best;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    labels
}

fn refine(
    n: usize,
    weights: &[Vec<f32>],
    node_sizes: &[usize],
    labels: &[usize],
    resolution: f32,
) -> Vec<usize> {
    let mut refined = vec![0_usize; n];
    for community in communities_from_labels(labels) {
        if community.len() == 1 {
            refined[community[0]] = community[0];
            continue;
        }

        let mut singleton_labels: Vec<usize> = (0..n).collect();
        for &node in &community {
            singleton_labels[node] = node;
        }
        let sub_labels = local_move(
            n,
            weights,
            node_sizes,
            singleton_labels,
            &community,
            resolution,
        );
        for &node in &community {
            refined[node] = sub_labels[node];
        }
    }
    refined
}

fn weight_to_community(
    node: usize,
    community: usize,
    weights: &[Vec<f32>],
    labels: &[usize],
    allowed: &[usize],
) -> f32 {
    allowed
        .iter()
        .copied()
        .filter(|&other| other != node && labels[other] == community)
        .map(|other| weights[node][other])
        .sum()
}

fn canonical_labels(n: usize, labels: &[usize]) -> Vec<usize> {
    let mut canonical = vec![0_usize; n];
    for community in communities_from_labels(labels) {
        let label = community[0];
        for node in community {
            canonical[node] = label;
        }
    }
    canonical
}

fn aggregate_weights(weights: &[Vec<f32>], groups: &[Vec<usize>]) -> Vec<Vec<f32>> {
    let mut aggregate = vec![vec![0.0_f32; groups.len()]; groups.len()];
    for (left_index, left) in groups.iter().enumerate() {
        for (right_index, right) in groups.iter().enumerate().skip(left_index + 1) {
            let mut weight = 0.0_f32;
            for &left_node in left {
                for &right_node in right {
                    weight += weights[left_node][right_node];
                }
            }
            aggregate[left_index][right_index] = weight;
            aggregate[right_index][left_index] = weight;
        }
    }
    aggregate
}

fn communities_from_labels(labels: &[usize]) -> Vec<Vec<usize>> {
    let mut groups = std::collections::BTreeMap::<usize, Vec<usize>>::new();
    for (node, &label) in labels.iter().enumerate() {
        groups.entry(label).or_default().push(node);
    }
    let mut communities: Vec<Vec<usize>> = groups.into_values().collect();
    for community in &mut communities {
        community.sort_unstable();
    }
    communities.sort_by_key(|community| community[0]);
    communities
}

fn is_singleton_partition(groups: &[Vec<usize>]) -> bool {
    groups
        .iter()
        .enumerate()
        .all(|(index, group)| group == &[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_merges() {
        let edges = [(0, 1, 0.86), (1, 2, 0.86), (0, 2, 0.86)];
        assert_eq!(leiden_cpm(3, &edges, 0.40), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn two_cliques_joined_by_one_bridge_edge_split() {
        let mut edges = Vec::new();
        for clique in [[0, 1, 2], [3, 4, 5]] {
            for i in 0..clique.len() {
                for j in (i + 1)..clique.len() {
                    edges.push((clique[i], clique[j], 0.85));
                }
            }
        }
        edges.push((2, 3, 0.85));

        assert_eq!(
            leiden_cpm(6, &edges, 0.40),
            vec![vec![0, 1, 2], vec![3, 4, 5]]
        );
    }

    #[test]
    fn empty_and_singleton_graphs() {
        assert!(leiden_cpm(0, &[], 0.40).is_empty());
        assert_eq!(leiden_cpm(1, &[], 0.40), vec![vec![0]]);
        assert_eq!(leiden_cpm(3, &[], 0.40), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn deterministic_across_repeated_calls() {
        let edges = [
            (0, 1, 0.85),
            (0, 2, 0.85),
            (1, 2, 0.85),
            (3, 4, 0.85),
            (3, 5, 0.85),
            (4, 5, 0.85),
            (2, 3, 0.85),
        ];
        let first = leiden_cpm(6, &edges, 0.40);
        for _ in 0..5 {
            assert_eq!(leiden_cpm(6, &edges, 0.40), first);
        }
    }
}
