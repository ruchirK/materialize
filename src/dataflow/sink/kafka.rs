// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::VecDeque;

use differential_dataflow::hashable::Hashable;
use log::error;
use rdkafka::config::ClientConfig;
use rdkafka::error::{KafkaError, RDKafkaError};
use rdkafka::producer::{BaseRecord, ThreadedProducer};
use timely::dataflow::channels::pact::Exchange;
use timely::dataflow::operators::generic::Operator;
use timely::dataflow::{Scope, Stream};

use dataflow_types::{Diff, KafkaSinkConnector, Timestamp};
use expr::GlobalId;
use interchange::avro::{DiffPair, Encoder};
use repr::{RelationDesc, Row};

static KAFKA_SINK_FUEL: usize = 10000;

// TODO@jldlaughlin: What guarantees does this sink support? #1728
pub fn kafka<G>(
    stream: &Stream<G, (Row, Timestamp, Diff)>,
    id: GlobalId,
    connector: KafkaSinkConnector,
    desc: RelationDesc,
) where
    G: Scope<Timestamp = Timestamp>,
{
    // We want exactly one worker to send all the data to the sink topic. We
    // achieve that by using an Exchange channel before the sink and mapping
    // all records for the sink to the sink's hash, which has the neat property
    // of also distributing sinks amongst workers
    let sink_hash = id.hashed();

    let encoder = Encoder::new(desc);
    let mut config = ClientConfig::new();
    config.set("bootstrap.servers", &connector.url.to_string());
    let producer: ThreadedProducer<_> = config.create().unwrap();
    let mut queue: VecDeque<(Row, Diff)> = VecDeque::new();
    let mut vector = Vec::new();
    let mut encoded_buffer = None;

    let name = format!("kafka-{}", id);
    stream.sink(
        Exchange::new(move |_| sink_hash),
        &name.clone(),
        move |input| {
            input.for_each(|_, rows| {
                rows.swap(&mut vector);

                for (row, _time, diff) in vector.drain(..) {
                    queue.push_back((row, diff));
                }
            });

            for _ in 0..KAFKA_SINK_FUEL {
                let (encoded, count) = if let Some((encoded, count)) = encoded_buffer.take() {
                    (encoded, count)
                } else {
                    if !queue.is_empty() {
                        let (row, diff) = queue.pop_front().expect("queue known to be nonempty");
                        let diff_pair = if diff < 0 {
                            DiffPair {
                                before: Some(&row),
                                after: None,
                            }
                        } else {
                            DiffPair {
                                before: None,
                                after: Some(&row),
                            }
                        };
                        let buf = encoder.encode(connector.schema_id, diff_pair);
                        (buf, diff.abs())
                    } else {
                        break;
                    }
                };

                let record = BaseRecord::<&Vec<u8>, _>::to(&connector.topic).payload(&encoded);
                if let Err((e, _)) = producer.send(record) {
                    error!("unable to produce in {}: {}", name, e);
                    match e {
                        KafkaError::MessageProduction(RDKafkaError::QueueFull) => {
                            encoded_buffer = Some((encoded, count));
                            return;
                        }
                        _ => (),
                    };
                }

                if count > 1 {
                    encoded_buffer = Some((encoded, count - 1));
                }
            }
            producer.flush(None);
        },
    )
}
