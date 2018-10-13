extern crate rand;
extern crate timely;
extern crate differential_dataflow;
extern crate core_affinity;

use rand::{Rng, SeedableRng, StdRng};

use timely::dataflow::*;
use timely::dataflow::operators::probe::Handle;

use differential_dataflow::input::Input;
use differential_dataflow::Collection;
use differential_dataflow::operators::*;
use differential_dataflow::lattice::Lattice;
// use differential_dataflow::operators::iterate::Variable;
use differential_dataflow::operators::arrange::ArrangeByKey;
use differential_dataflow::operators::arrange::ArrangeBySelf;

type Node = usize;

fn main() {

    let nodes: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    let edges: usize = std::env::args().nth(2).unwrap().parse().unwrap();
    let rate: usize  = std::env::args().nth(3).unwrap().parse().unwrap();
    let goal: usize  = std::env::args().nth(4).unwrap().parse().unwrap();
    let mode: usize = std::env::args().nth(5).unwrap().parse().unwrap();

    // Our setting involves four read query types, and two updatable base relations.
    //
    //  Q1: Point lookup: reads "state" associated with a node.
    //  Q2: One-hop lookup: reads "state" associated with neighbors of a node.
    //  Q3: Two-hop lookup: reads "state" associated with n-of-n's of a node.
    //  Q4: Shortest path: reports hop count between two query nodes.
    //
    //  R1: "State": a pair of (node, T) for some type T that I don't currently know.
    //  R2: "Graph": pairs (node, node) indicating linkage between the two nodes.

    timely::execute_from_args(std::env::args().skip(3), move |worker| {

        let index = worker.index();
        let peers = worker.peers();
        let timer = ::std::time::Instant::now();

        let core_ids = core_affinity::get_core_ids().unwrap();
        core_affinity::set_for_current(core_ids[index % core_ids.len()]);

        // define BFS dataflow; return handles to roots and edges inputs
        let mut probe = Handle::new();

        let (mut query, mut state, mut graph) = worker.dataflow(|scope| {

            let (query_input, query) = scope.new_collection();
            let (state_input, state) = scope.new_collection();
            let (graph_input, graph) = scope.new_collection();

            let state_indexed = state.arrange_by_key();
            let graph_indexed = graph.map(|(src, dst)| (dst, src))
                                     .concat(&graph)
                                     .arrange_by_key();

            match mode {

                1 => {
                    // Q1: Point lookups on `state`:
                    query
                        .map(|(x,_)| x)
                        .arrange_by_self()
                        .join_core(&state_indexed, |&query, &(), &state| Some((query, state)))
                        .probe_with(&mut probe);
                },
                2 => {
                    // Q2: One-hop lookups on `state`:
                    query
                        .map(|(x,_)| x)
                        .arrange_by_self()
                        .join_core(&graph_indexed, |&query, &(), &friend| Some((friend, query)))
                        .join_core(&state_indexed, |_friend, &query, &state| Some((query, state)))
                        .probe_with(&mut probe);
                },
                3 => {
                    // Q3: Two-hop lookups on `state`:
                    query
                        .map(|(x,_)| x)
                        .arrange_by_self()
                        .join_core(&graph_indexed, |&query, &(), &friend| Some((friend, query)))
                        .join_core(&graph_indexed, |_friend, &query, &friend2| Some((friend2, query)))
                        .join_core(&state_indexed, |_friend2, &query, &state| Some((query, state)))
                        .probe_with(&mut probe);
                },
                4 => {
                    // Q4: Shortest path queries:
                    three_hop(&graph_indexed, &graph_indexed, &query)
                        .probe_with(&mut probe);
                }
                x => { panic!("Unknown mode: {:?}; must be: 1, 2, 3, 4", x); }
            }

            (query_input, state_input, graph_input)
        });

        let seed: &[_] = &[1, 2, 3, index];
        let mut rng1: StdRng = SeedableRng::from_seed(seed);    // rng for edge additions
        let mut rng2: StdRng = SeedableRng::from_seed(seed);    // rng for edge deletions
        let seed: &[_] = &[1, 2, 4, index];
        let mut rng3: StdRng = SeedableRng::from_seed(seed);    // rng for query additions
        let mut rng4: StdRng = SeedableRng::from_seed(seed);    // rng for q1 deletions

        if index == 0 { println!("performing workload on random graph with {} nodes, {} edges:", nodes, edges); }

        let worker_edges = edges/peers + if index < (edges % peers) { 1 } else { 0 };
        for _ in 0 .. worker_edges {
            graph.insert((rng1.gen_range(0, nodes), rng1.gen_range(0, nodes)));
        }
        for node in 0 .. nodes {
            if node % peers == index {
                state.insert((node, node));
            }
        }

        let worker_window = 1;
        for _ in 0 .. worker_window {
            query.insert((rng3.gen_range(0, nodes), rng3.gen_range(0, nodes)));
        }

        query.advance_to(1);                  query.flush();
        state.advance_to(usize::max_value()); state.flush();
        graph.advance_to(1);                  graph.flush();

        // finish graph loading work.
        while probe.less_than(graph.time()) { worker.step(); }

        if index == 0 { println!("{:?}\tgraph loaded", timer.elapsed()); }

        let requests_per_sec = rate / 2;
        let ns_per_request = 1_000_000_000 / requests_per_sec;
        let mut request_counter = peers + index;    // skip first request for each.
        let mut ack_counter = peers + index;

        let mut inserted_ns = 1;

        let timer = ::std::time::Instant::now();
        let mut counts = vec![[0usize; 16]; 64];

        let ack_target = goal * rate;
        while ack_counter < ack_target {

            // Open-loop latency-throughput test, parameterized by offered rate `ns_per_request`.
            let elapsed = timer.elapsed();
            let elapsed_ns: usize = (elapsed.as_secs() * 1_000_000_000 + (elapsed.subsec_nanos() as u64)) as usize;

            // Determine completed ns.
            let acknowledged_ns: usize = probe.with_frontier(|frontier| frontier[0]);

            // any un-recorded measurements that are complete should be recorded.
            while (ack_counter * ns_per_request) < acknowledged_ns && ack_counter < ack_target {
                let requested_at = ack_counter * ns_per_request;
                let count_index = (elapsed_ns - requested_at).next_power_of_two().trailing_zeros() as usize;
                if ack_counter > ack_target / 2 {
                    let low_bits = ((elapsed_ns - requested_at) >> (count_index - 5)) & 0xF;
                    counts[count_index][low_bits as usize] += 1;
                }
                ack_counter += peers;
            }

            // Now, should we introduce more records before stepping the worker?
            //
            // Thinking: inserted_ns - acknowledged_ns is some amount of time that
            // is currently outstanding in the system, and we needn't advance our
            // inputs unless by this order of magnitude.
            //
            // The more sophisticated plan is: we compute the next power of two
            // greater than inserted_ns - acknowledged_ns and look for the last
            // multiple of this number in the interval [inserted_ns, elapsed_ns].
            // If such a multiple exists, we introduce records to that point and
            // advance the input.

            // let scale = (inserted_ns - acknowledged_ns).next_power_of_two();
            // let target_ns = elapsed_ns & !(scale - 1);

            let mut target_ns = elapsed_ns & !((1 << 20) - 1);

            // let mut target_ns = if acknowledged_ns >= inserted_ns { elapsed_ns } else { inserted_ns };

            if target_ns > inserted_ns + 1_000_000_000 { target_ns = inserted_ns + 1_000_000_000; }

            if inserted_ns < target_ns {

                while (request_counter * ns_per_request) < target_ns {

                    if (request_counter / peers) % 2 == 0 {
                        graph.advance_to(request_counter * ns_per_request);
                        graph.insert((rng1.gen_range(0, nodes),rng1.gen_range(0, nodes)));
                        graph.remove((rng2.gen_range(0, nodes),rng2.gen_range(0, nodes)));
                    }
                    else {
                        query.advance_to(request_counter * ns_per_request);
                        query.insert((rng3.gen_range(0, nodes),rng3.gen_range(0, nodes)));
                        query.remove((rng4.gen_range(0, nodes),rng4.gen_range(0, nodes)));
                    }
                    request_counter += peers;
                }
                graph.advance_to(target_ns); graph.flush();
                query.advance_to(target_ns); query.flush();
                inserted_ns = target_ns;
            }

            worker.step();
        }

        if index == 0 {

            let mut results = Vec::new();
            let total = counts.iter().map(|x| x.iter().sum::<usize>()).sum();
            let mut sum = 0;
            for index in (10 .. counts.len()).rev() {
                for sub in (0 .. 16).rev() {
                    if sum > 0 && sum < total {
                        let latency = (1 << (index-1)) + (sub << (index-5));
                        let fraction = (sum as f64) / (total as f64);
                        results.push((latency, fraction));
                    }
                    sum += counts[index][sub];
                }
            }
            for (latency, fraction) in results.drain(..).rev() {
                println!("{}\t{}", latency, fraction);
            }
        }

    }).unwrap();
}

use differential_dataflow::trace::implementations::ord::OrdValSpine as DefaultValTrace;
use differential_dataflow::operators::arrange::TraceAgent;
use differential_dataflow::operators::arrange::Arranged;

type Arrange<G, K, V, R> = Arranged<G, K, V, R, TraceAgent<K, V, <G as ScopeParent>::Timestamp, R, DefaultValTrace<K, V, <G as ScopeParent>::Timestamp, R>>>;

// returns pairs (n, s) indicating node n can be reached from a root in s steps.
fn three_hop<G: Scope>(
    forward_graph: &Arrange<G, Node, Node, isize>,
    reverse_graph: &Arrange<G, Node, Node, isize>,
    goals: &Collection<G, (Node, Node)>) -> Collection<G, ((Node, Node), u32)>
where G::Timestamp: Lattice+Ord {

    let sources = goals.map(|(x,_)| x);
    let targets = goals.map(|(_,y)| y);

    // Q3: Two-hop lookups on `state`:
    let forward0 = sources.map(|x| (x, (x,0)));
    let forward1 = forward0.join_core(&forward_graph, |&_, &(source,dist), &friend| Some((friend, (source, dist+1))));
    let forward2 = forward1.join_core(&forward_graph, |&_, &(source,dist), &friend| Some((friend, (source, dist+1))));

    let reverse0 = targets.map(|x| (x, (x,0)));
    let reverse1 = reverse0.join_core(&reverse_graph, |&_, &(target,dist), &friend| Some((friend, (target, dist+1))));
    let reverse2 = reverse1.join_core(&reverse_graph, |&_, &(target,dist), &friend| Some((friend, (target, dist+1))));

    let forward = forward0.concat(&forward1).concat(&forward2);
    let reverse = reverse0.concat(&reverse1).concat(&reverse2);

    forward
        .join_map(&reverse, |_,&(source, dist1),&(target, dist2)| ((source, target), dist1 + dist2))
        .group(|_st,input,output| output.push((*input[0].0,1)))
}

// // returns pairs (n, s) indicating node n can be reached from a root in s steps.
// fn bidijkstra<G: Scope>(
//     forward_graph: &Arrange<G, Node, Node, isize>,
//     reverse_graph: &Arrange<G, Node, Node, isize>,
//     goals: &Collection<G, (Node, Node)>,
//     bound: u64) -> Collection<G, ((Node, Node), u32)>
// where G::Timestamp: Lattice+Ord {

//     goals.scope().scoped(|inner| {

//         // Our plan is to start evolving distances from both sources and destinations.
//         // The evolution from a source or destination should continue as long as there
//         // is a corresponding destination or source that has not yet been reached.

//         // forward and reverse (node, (root, dist))
//         let forward = Variable::from_args(bound, 1, goals.map(|(x,_)| (x,(x,0))).enter(inner));
//         let reverse = Variable::from_args(bound, 1, goals.map(|(_,y)| (y,(y,0))).enter(inner));

//         let goals = goals.enter(inner);
//         let forward_graph = forward_graph.enter(inner);
//         let reverse_graph = reverse_graph.enter(inner);

//         // Let's determine which (src, dst) pairs are ready to return.
//         //
//         //   done(src, dst) := forward(src, med), reverse(dst, med), goal(src, dst).
//         //
//         // This is a cyclic join, which should scare us a bunch.
//         let reached =
//         forward
//             .join_map(&reverse, |_, &(src,d1), &(dst,d2)| ((src, dst), d1 + d2))
//             .group(|_key, s, t| t.push((*s[0].0, 1)))
//             .semijoin(&goals);

//         let active =
//         reached
//             .negate()
//             .map(|(srcdst,_)| srcdst)
//             .concat(&goals)
//             .consolidate();

//         // Let's expand out forward queries that are active.
//         let forward_active = active.map(|(x,_y)| x).distinct();
//         let forward_next =
//         forward
//             .map(|(med, (src, dist))| (src, (med, dist)))
//             .semijoin(&forward_active)
//             .map(|(src, (med, dist))| (med, (src, dist)))
//             .join_core(&forward_graph, |_med, &(src, dist), &next| Some((next, (src, dist+1))))
//             .concat(&forward)
//             .map(|(next, (src, dist))| ((next, src), dist))
//             .group(|_key, s, t| t.push((*s[0].0, 1)))
//             .map(|((next, src), dist)| (next, (src, dist)));

//         forward.set(&forward_next);

//         // Let's expand out reverse queries that are active.
//         let reverse_active = active.map(|(_x,y)| y).distinct();
//         let reverse_next =
//         reverse
//             .map(|(med, (rev, dist))| (rev, (med, dist)))
//             .semijoin(&reverse_active)
//             .map(|(rev, (med, dist))| (med, (rev, dist)))
//             .join_core(&reverse_graph, |_med, &(rev, dist), &next| Some((next, (rev, dist+1))))
//             .concat(&reverse)
//             .map(|(next, (rev, dist))| ((next, rev), dist))
//             .group(|_key, s, t| t.push((*s[0].0, 1)))
//             .map(|((next,rev), dist)| (next, (rev, dist)));

//         reverse.set(&reverse_next);

//         reached.leave()
//     })
// }