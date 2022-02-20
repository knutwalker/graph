use log::info;
use std::{collections::HashMap, hash::Hash, sync::atomic::Ordering, time::Instant};

use crate::{dss::DisjointSetStruct, prelude::*};
use rayon::prelude::*;

// Number of nodes to be processed in batch by a single thread.
const CHUNK_SIZE: usize = 16384;
// The number of relationships of each node to sample during subgraph sampling.
const NEIGHBOR_ROUNDS: usize = 2;
// The number of samples from the DSS to find the largest component.
const SAMPLING_SIZE: usize = 1024;

pub fn wcc_par_iter<NI: Idx>(graph: &DirectedCsrGraph<NI>) -> DisjointSetStruct<NI> {
    let node_count = graph.node_count().index();
    let dss = DisjointSetStruct::new(node_count);

    (0..node_count).into_par_iter().map(NI::new).for_each(|u| {
        graph.out_neighbors(u).iter().for_each(|v| dss.union(u, *v));
    });

    dss
}

pub fn wcc_rayon_chunks<NI: Idx>(graph: &DirectedCsrGraph<NI>) -> DisjointSetStruct<NI> {
    let node_count = graph.node_count().index();
    let dss = DisjointSetStruct::new(node_count);

    (0..node_count)
        .into_par_iter()
        .chunks(CHUNK_SIZE)
        .for_each(|chunk| {
            for u in chunk {
                let u = NI::new(u);
                graph.out_neighbors(u).iter().for_each(|v| dss.union(u, *v));
            }
        });

    dss
}

pub fn wcc_manual_chunks<NI: Idx>(graph: &DirectedCsrGraph<NI>) -> DisjointSetStruct<NI> {
    let node_count = graph.node_count().index();
    let dss = DisjointSetStruct::new(node_count);

    let next_chunk = NI::zero().atomic();

    rayon::scope(|s| {
        for _ in 0..rayon::current_num_threads() {
            s.spawn(|_| loop {
                let start = next_chunk.fetch_add(NI::new(CHUNK_SIZE), Ordering::AcqRel);
                if start >= graph.node_count() {
                    break;
                }

                let end = (start + NI::new(CHUNK_SIZE)).min(graph.node_count());

                for u in start..end {
                    for v in graph.out_neighbors(u) {
                        dss.union(u, *v);
                    }
                }
            });
        }
    });

    dss
}

pub fn wcc_single_thread<NI: Idx>(graph: &DirectedCsrGraph<NI>) -> DisjointSetStruct<NI> {
    let dss = DisjointSetStruct::new(graph.node_count().index());

    for u in 0..graph.node_count().index() {
        let u = NI::new(u);
        for v in graph.out_neighbors(u) {
            dss.union(u, *v);
        }
    }

    dss
}

pub fn wcc_std_threads<NI: Idx>(graph: &DirectedCsrGraph<NI>) -> DisjointSetStruct<NI> {
    let next_chunk = NI::zero().atomic();
    let dss = DisjointSetStruct::new(graph.node_count().index());

    easy_parallel::Parallel::new()
        .each(0..num_cpus::get(), |_| loop {
            let start = next_chunk.fetch_add(NI::new(CHUNK_SIZE), Ordering::AcqRel);
            if start >= graph.node_count() {
                break;
            }

            let end = (start + NI::new(CHUNK_SIZE)).min(graph.node_count());

            for u in start..end {
                for v in graph.out_neighbors(u) {
                    dss.union(u, *v);
                }
            }
        })
        .run();

    dss
}

pub fn wcc<NI: Idx + Hash>(graph: &DirectedCsrGraph<NI>) -> DisjointSetStruct<NI> {
    let start = Instant::now();
    let dss = DisjointSetStruct::new(graph.node_count().index());
    info!("DSS creation took {} ms.", start.elapsed().as_millis());

    let start = Instant::now();
    sample_subgraph(graph, &dss);
    info!("Link subgraph took {} ms.", start.elapsed().as_millis());

    let start = Instant::now();
    let largest_component = find_largest_component(&dss);
    info!("Get component took {} ms.", start.elapsed().as_millis());

    let start = Instant::now();
    link_remaining(graph, &dss, largest_component);
    info!("Link remaining took {} ms.", start.elapsed().as_millis());

    dss
}

// Sample a subgraph by looking at the first `NEIGHBOR_ROUNDS` many targets of each node.
fn sample_subgraph<NI: Idx>(graph: &DirectedCsrGraph<NI>, dss: &DisjointSetStruct<NI>) {
    (0..graph.node_count().index())
        .into_par_iter()
        .chunks(CHUNK_SIZE)
        .for_each(|chunk| {
            for u in chunk {
                let u = NI::new(u);
                let limit = usize::min(graph.out_degree(u).index(), NEIGHBOR_ROUNDS);

                for v in &graph.out_neighbors(u)[..limit] {
                    dss.union(u, *v);
                }
            }
        });
}

// Find the largest component after running wcc on the sampled graph.
fn find_largest_component<NI: Idx + Hash>(dss: &DisjointSetStruct<NI>) -> NI {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut sample_counts = HashMap::<NI, usize>::new();

    for _ in 0..SAMPLING_SIZE {
        let component = dss.find(NI::new(rng.gen_range(0..dss.len())));
        let count = sample_counts.entry(component).or_insert(0);
        *count += 1;
    }

    let (most_frequent, size) = sample_counts
        .iter()
        .max_by(|(_, v1), (_, v2)| v1.cmp(v2))
        .unwrap();

    info!(
        "Largest intermediate component {most_frequent:?} containing approx. {}% of the graph.",
        (*size as f32 / SAMPLING_SIZE as f32 * 100.0) as usize
    );

    *most_frequent
}

// Process the remaining edges while skipping nodes that are in the largest component.
fn link_remaining<NI: Idx>(
    graph: &DirectedCsrGraph<NI>,
    dss: &DisjointSetStruct<NI>,
    skip_component: NI,
) {
    (0..graph.node_count().index())
        .into_par_iter()
        .chunks(CHUNK_SIZE)
        .for_each(|chunk| {
            for u in chunk {
                let u = NI::new(u);
                if dss.find(u) == skip_component {
                    continue;
                }

                if graph.out_degree(u).index() > NEIGHBOR_ROUNDS {
                    for v in &graph.out_neighbors(u)[NEIGHBOR_ROUNDS..] {
                        dss.union(u, *v);
                    }
                }

                for v in graph.in_neighbors(u) {
                    dss.union(u, *v);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_components() {
        let graph: DirectedCsrGraph<usize> =
            GraphBuilder::new().edges(vec![(0, 1), (2, 3)]).build();

        let dss = wcc(&graph);

        assert_eq!(dss.find(0), dss.find(1));
        assert_eq!(dss.find(2), dss.find(3));
        assert_ne!(dss.find(1), dss.find(2));
    }
}
