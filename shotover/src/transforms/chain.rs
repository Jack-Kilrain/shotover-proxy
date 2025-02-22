use crate::message::Messages;
use crate::transforms::{TransformBuilder, Transforms, Wrapper};
use anyhow::{anyhow, Result};
use derivative::Derivative;
use futures::TryFutureExt;
use metrics::{histogram, register_counter, register_histogram, Counter, Histogram};
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, trace, Instrument};

type InnerChain = Vec<TransformAndMetrics>;

#[derive(Debug)]
pub struct BufferedChainMessages {
    pub local_addr: SocketAddr,
    pub messages: Messages,
    pub flush: bool,
    pub return_chan: Option<oneshot::Sender<Result<Messages>>>,
}

impl BufferedChainMessages {
    pub fn new_with_no_return(m: Messages, local_addr: SocketAddr) -> Self {
        BufferedChainMessages {
            local_addr,
            messages: m,
            flush: false,
            return_chan: None,
        }
    }

    pub fn new(
        m: Messages,
        local_addr: SocketAddr,
        flush: bool,
        return_chan: oneshot::Sender<Result<Messages>>,
    ) -> Self {
        BufferedChainMessages {
            local_addr,
            messages: m,
            flush,
            return_chan: Some(return_chan),
        }
    }
}

//TODO explore running the transform chain on a LocalSet for better locality to a given OS thread
//Will also mean we can have `!Send` types  in our transform chain

/// A transform chain is a ordered list of transforms that a message will pass through.
/// Transform chains can be of arbitary complexity and a transform can even have its own set of child transform chains.
/// Transform chains are defined by the user in Shotover's configuration file and are linked to sources.
///
/// The transform chain is a vector of mutable references to the enum [Transforms] (which is an enum dispatch wrapper around the various transform types).
#[derive(Derivative)]
#[derivative(Debug)]
pub struct TransformChain {
    pub name: &'static str,
    pub chain: InnerChain,

    #[derivative(Debug = "ignore")]
    chain_total: Counter,
    #[derivative(Debug = "ignore")]
    chain_failures: Counter,
    #[derivative(Debug = "ignore")]
    chain_batch_size: Histogram,
}

#[derive(Debug, Clone)]
pub struct BufferedChain {
    send_handle: mpsc::Sender<BufferedChainMessages>,
    #[cfg(test)]
    pub count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl BufferedChain {
    pub async fn process_request(
        &mut self,
        wrapper: Wrapper<'_>,
        buffer_timeout_micros: Option<u64>,
    ) -> Result<Messages> {
        self.process_request_with_receiver(wrapper, buffer_timeout_micros)
            .await?
            .await?
    }

    async fn process_request_with_receiver(
        &mut self,
        wrapper: Wrapper<'_>,
        buffer_timeout_micros: Option<u64>,
    ) -> Result<oneshot::Receiver<Result<Messages>>> {
        let (one_tx, one_rx) = oneshot::channel::<Result<Messages>>();
        match buffer_timeout_micros {
            None => {
                self.send_handle
                    .send(BufferedChainMessages::new(
                        wrapper.requests,
                        wrapper.local_addr,
                        wrapper.flush,
                        one_tx,
                    ))
                    .map_err(|e| anyhow!("Couldn't send message to wrapped chain {:?}", e))
                    .await?
            }
            Some(timeout) => {
                self.send_handle
                    .send_timeout(
                        BufferedChainMessages::new(
                            wrapper.requests,
                            wrapper.local_addr,
                            wrapper.flush,
                            one_tx,
                        ),
                        Duration::from_micros(timeout),
                    )
                    .map_err(|e| anyhow!("Couldn't send message to wrapped chain {:?}", e))
                    .await?
            }
        }

        Ok(one_rx)
    }

    pub async fn process_request_no_return(
        &mut self,
        wrapper: Wrapper<'_>,
        buffer_timeout_micros: Option<u64>,
    ) -> Result<()> {
        if wrapper.flush {
            // To obey flush request we need to ensure messages have completed sending before returning.
            // In order to achieve that we need to use the regular process_request method.
            self.process_request(wrapper, buffer_timeout_micros).await?;
        } else {
            // When there is no flush we can return much earlier by not waiting for a response.
            match buffer_timeout_micros {
                None => {
                    self.send_handle
                        .send(BufferedChainMessages::new_with_no_return(
                            wrapper.requests,
                            wrapper.local_addr,
                        ))
                        .map_err(|e| anyhow!("Couldn't send message to wrapped chain {:?}", e))
                        .await?
                }
                Some(timeout) => {
                    self.send_handle
                        .send_timeout(
                            BufferedChainMessages::new_with_no_return(
                                wrapper.requests,
                                wrapper.local_addr,
                            ),
                            Duration::from_micros(timeout),
                        )
                        .map_err(|e| anyhow!("Couldn't send message to wrapped chain {:?}", e))
                        .await?
                }
            }
        }
        Ok(())
    }
}

impl TransformChain {
    pub async fn process_request(&mut self, mut wrapper: Wrapper<'_>) -> Result<Messages> {
        let start = Instant::now();
        wrapper.reset(&mut self.chain);

        self.chain_batch_size.record(wrapper.requests.len() as f64);
        let client_details = wrapper.client_details.to_owned();
        let result = wrapper.call_next_transform().await;
        self.chain_total.increment(1);
        if result.is_err() {
            self.chain_failures.increment(1);
        }

        histogram!("shotover_chain_latency_seconds", start.elapsed(),  "chain" => self.name, "client_details" => client_details);
        result
    }

    pub async fn process_request_rev(&mut self, mut wrapper: Wrapper<'_>) -> Result<Messages> {
        let start = Instant::now();
        wrapper.reset_rev(&mut self.chain);

        self.chain_batch_size.record(wrapper.requests.len() as f64);
        let client_details = wrapper.client_details.to_owned();
        let result = wrapper.call_next_transform_pushed().await;
        self.chain_total.increment(1);
        if result.is_err() {
            self.chain_failures.increment(1);
        }

        histogram!("shotover_chain_latency_seconds", start.elapsed(),  "chain" => self.name, "client_details" => client_details);
        result
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct TransformAndMetrics {
    pub transform: Transforms,
    #[derivative(Debug = "ignore")]
    pub transform_total: Counter,
    #[derivative(Debug = "ignore")]
    pub transform_failures: Counter,
    #[derivative(Debug = "ignore")]
    pub transform_latency: Histogram,
    #[derivative(Debug = "ignore")]
    pub transform_pushed_total: Counter,
    #[derivative(Debug = "ignore")]
    pub transform_pushed_failures: Counter,
    #[derivative(Debug = "ignore")]
    pub transform_pushed_latency: Histogram,
}

impl TransformAndMetrics {
    #[cfg(test)]
    pub fn new(transform: Transforms) -> Self {
        TransformAndMetrics {
            transform,
            transform_total: Counter::noop(),
            transform_failures: Counter::noop(),
            transform_latency: Histogram::noop(),
            transform_pushed_total: Counter::noop(),
            transform_pushed_failures: Counter::noop(),
            transform_pushed_latency: Histogram::noop(),
        }
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct TransformBuilderAndMetrics {
    pub builder: Box<dyn TransformBuilder>,
    #[derivative(Debug = "ignore")]
    transform_total: Counter,
    #[derivative(Debug = "ignore")]
    transform_failures: Counter,
    #[derivative(Debug = "ignore")]
    transform_latency: Histogram,
    #[derivative(Debug = "ignore")]
    transform_pushed_total: Counter,
    #[derivative(Debug = "ignore")]
    transform_pushed_failures: Counter,
    #[derivative(Debug = "ignore")]
    transform_pushed_latency: Histogram,
}

impl TransformBuilderAndMetrics {
    fn build(&self) -> TransformAndMetrics {
        TransformAndMetrics {
            transform: self.builder.build(),
            transform_total: self.transform_total.clone(),
            transform_failures: self.transform_failures.clone(),
            transform_latency: self.transform_latency.clone(),
            transform_pushed_total: self.transform_pushed_total.clone(),
            transform_pushed_failures: self.transform_pushed_failures.clone(),
            transform_pushed_latency: self.transform_pushed_latency.clone(),
        }
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct TransformChainBuilder {
    pub name: &'static str,
    pub chain: Vec<TransformBuilderAndMetrics>,

    #[derivative(Debug = "ignore")]
    chain_total: Counter,
    #[derivative(Debug = "ignore")]
    chain_failures: Counter,
    #[derivative(Debug = "ignore")]
    chain_batch_size: Histogram,
}

impl TransformChainBuilder {
    pub fn new(chain: Vec<Box<dyn TransformBuilder>>, name: &'static str) -> Self {
        let chain = chain.into_iter().map(|builder|
            TransformBuilderAndMetrics {
                transform_total: register_counter!("shotover_transform_total_count", "transform" => builder.get_name()),
                transform_failures: register_counter!("shotover_transform_failures_count", "transform" => builder.get_name()),
                transform_latency: register_histogram!("shotover_transform_latency_seconds", "transform" => builder.get_name()),
                transform_pushed_total: register_counter!("shotover_transform_pushed_total_count", "transform" => builder.get_name()),
                transform_pushed_failures: register_counter!("shotover_transform_pushed_failures_count", "transform" => builder.get_name()),
                transform_pushed_latency: register_histogram!("shotover_transform_pushed_latency_seconds", "transform" => builder.get_name()),
                builder,
            }
        ).collect();

        let chain_batch_size =
            register_histogram!("shotover_chain_messages_per_batch_count", "chain" => name);
        let chain_total = register_counter!("shotover_chain_total_count", "chain" => name);
        let chain_failures = register_counter!("shotover_chain_failures_count", "chain" => name);
        // Cant register shotover_chain_latency_seconds because a unique one is created for each client ip address

        TransformChainBuilder {
            name,
            chain,
            chain_total,
            chain_failures,
            chain_batch_size,
        }
    }

    pub fn validate(&self) -> Vec<String> {
        if self.chain.is_empty() {
            return vec![
                format!("{} chain:", self.name),
                "  Chain cannot be empty".to_string(),
            ];
        }

        let last_index = self.chain.len() - 1;

        let mut errors = self
            .chain
            .iter()
            .enumerate()
            .flat_map(|(i, transform)| {
                let mut errors = vec![];

                if i == last_index && !transform.builder.is_terminating() {
                    errors.push(format!(
                        "  Non-terminating transform {:?} is last in chain. Last transform must be terminating.",
                        transform.builder.get_name()
                    ));
                } else if i != last_index && transform.builder.is_terminating() {
                    errors.push(format!(
                        "  Terminating transform {:?} is not last in chain. Terminating transform must be last in chain.",
                        transform.builder.get_name()
                    ));
                }

                errors.extend(transform.builder.validate().iter().map(|x| format!("  {x}")));

                errors
            })
            .collect::<Vec<String>>();

        if !errors.is_empty() {
            errors.insert(0, format!("{} chain:", self.name));
        }

        errors
    }

    pub fn build_buffered(&self, buffer_size: usize) -> BufferedChain {
        let (tx, mut rx) = mpsc::channel::<BufferedChainMessages>(buffer_size);

        #[cfg(test)]
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(test)]
        let count_clone = count.clone();

        // Even though we don't keep the join handle, this thread will wrap up once all corresponding senders have been dropped.

        let mut chain = self.build();
        let _jh = tokio::spawn(
            async move {
                while let Some(BufferedChainMessages {
                    local_addr,
                    return_chan,
                    messages,
                    flush,
                }) = rx.recv().await
                {
                    #[cfg(test)]
                    {
                        count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }

                    let mut wrapper = Wrapper::new_with_addr(messages, local_addr);
                    wrapper.flush = flush;
                    let chain_response = chain.process_request(wrapper).await;

                    if let Err(e) = &chain_response {
                        error!("Internal error in buffered chain: {e:?}");
                    };

                    match return_chan {
                        None => trace!("Ignoring response due to lack of return chan"),
                        Some(tx) => {
                            if let Err(message) = tx.send(chain_response) {
                                trace!("Failed to send response message over return chan. Message was: {message:?}");
                            }
                        }
                    };
                }

                debug!("buffered chain processing thread exiting, stopping chain loop and dropping");

                match chain
                    .process_request(Wrapper::flush())
                    .await
                {
                    Ok(_) => info!("Buffered chain {} was shutdown", chain.name),
                    Err(e) => error!(
                        "Buffered chain {} encountered an error when flushing the chain for shutdown: {}",
                        chain.name, e
                    ),
                }
            }
            .in_current_span(),
        );

        BufferedChain {
            send_handle: tx,
            #[cfg(test)]
            count,
        }
    }

    /// Clone the chain while adding a producer for the pushed messages channel
    pub fn build(&self) -> TransformChain {
        let chain = self.chain.iter().map(|x| x.build()).collect();

        TransformChain {
            name: self.name,
            chain,
            chain_total: self.chain_total.clone(),
            chain_failures: self.chain_failures.clone(),
            chain_batch_size: self.chain_batch_size.clone(),
        }
    }

    /// Clone the chain while adding a producer for the pushed messages channel
    pub fn build_with_pushed_messages(
        &self,
        pushed_messages_tx: mpsc::UnboundedSender<Messages>,
    ) -> TransformChain {
        let chain = self
            .chain
            .iter()
            .map(|x| {
                let mut transform = x.build();
                transform
                    .transform
                    .set_pushed_messages_tx(pushed_messages_tx.clone());
                transform
            })
            .collect();

        TransformChain {
            name: self.name,
            chain,
            chain_total: self.chain_total.clone(),
            chain_failures: self.chain_failures.clone(),
            chain_batch_size: self.chain_batch_size.clone(),
        }
    }
}

#[cfg(test)]
mod chain_tests {
    use crate::transforms::chain::TransformChainBuilder;
    use crate::transforms::debug::printer::DebugPrinter;
    use crate::transforms::null::NullSink;

    #[tokio::test]
    async fn test_validate_invalid_chain() {
        let chain = TransformChainBuilder::new(vec![], "test-chain");
        assert_eq!(
            chain.validate(),
            vec!["test-chain chain:", "  Chain cannot be empty"]
        );
    }

    #[tokio::test]
    async fn test_validate_valid_chain() {
        let chain = TransformChainBuilder::new(
            vec![
                Box::<DebugPrinter>::default(),
                Box::<DebugPrinter>::default(),
                Box::<NullSink>::default(),
            ],
            "test-chain",
        );
        assert_eq!(chain.validate(), Vec::<String>::new());
    }
}
