use crate::config::Configuration;
use crate::debounced_pool::{DebouncedPool, DebouncedPoolUpdate};
use anyhow::Result;
use futures::{stream, Stream, StreamExt as _1};
use log::info;
use static_assertions::assert_impl_all;
use std::sync::{Arc, Mutex};
use tokio::stream::StreamExt as _2;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Raw update messages that can come from the rest of the service,
/// and are used to update the current connections state,
/// sending uptime tracking events as needed.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateMessage {
    GuildOnline(u64),
    GuildOffline(u64),
    QueueOnline,
    QueueOffline,
    GatewayOnline,
    GatewayOffline,
    GatewayHeartbeat,
}

/// Represents a bulk uptime event that is dispatched to the uptime service
#[derive(Clone, Debug, PartialEq)]
enum UptimeEvent {
    Online(Vec<u64>),
    Offline(Vec<u64>),
    Heartbeat(Vec<u64>),
}

impl UptimeEvent {
    fn from_pool_update(update: DebouncedPoolUpdate<u64>) -> Vec<Self> {
        let mut updates = Vec::new();
        if let Some(added) = update.added {
            updates.push(Self::Online(added));
        }
        if let Some(removed) = update.removed {
            updates.push(Self::Offline(removed));
        }

        updates
    }
}

/// Represents an uptime tracking handler for the ingress service,
/// deriving a stateful status of the connection to each external service
/// and using that to inform heartbeat/online/offline events sent to an uptime tracking service
/// on a per-guild basis.
/// This information is then used to inform scheduled indexing jobs.
///
/// When creating, gives a multi-producer channel that can be used to send updates
/// to the uptime tracking handler
pub struct Tracker {
    updates: UnboundedReceiver<UpdateMessage>,
    debounced_guild_updates: UnboundedReceiver<DebouncedPoolUpdate<u64>>,
    state: TrackerState,
}

impl Tracker {
    /// Creates a new tracker from the configuration,
    /// also giving a multi-producer clone-able channel that can be used to send updates
    pub fn new(config: Arc<Configuration>) -> (Self, UnboundedSender<UpdateMessage>) {
        let (update_sender, update_receiver) = mpsc::unbounded_channel::<UpdateMessage>();
        let (active_guilds, debounced_guild_updates) =
            DebouncedPool::new(config.guild_uptime_debounce_delay.clone());
        let new_tracker = Self {
            updates: update_receiver,
            debounced_guild_updates,
            state: TrackerState {
                config,
                active_guilds,
                connection_status: Arc::new(Mutex::new(ConnectionStatus::new())),
            },
        };
        (new_tracker, update_sender)
    }

    /// Runs the tracker to completion, listening for updates in the channel
    /// and returning early if an error occurs with connecting to the uptime service initially
    pub async fn run(self) -> Result<()> {
        // First, connect to the uptime tracking service
        // TODO implement

        // Pipe uptime events to uptime service
        self.stream_events()
            .for_each(|event| async move {
                // Note: we measure the time received at the sink,
                // but the timing doesn't really matter that much as long as it is measured
                // before a potential retry loop
                // (the propagation delay between the stream processors
                // is generally <250ms even if debounced)
                let timestamp = architus_id::time::millisecond_ts();

                // TODO implement
                info!("Uptime event at {}: {:?}", timestamp, event);
            })
            .await;

        Ok(())
    }

    /// Listen for incoming updates and use them to update the internal state.
    /// Emits outgoing uptime events to be forwarded to the uptime service
    fn stream_events(self) -> impl Stream<Item = UptimeEvent> {
        let uptime_events = self.state.pipe_updates(self.updates);
        let debounced_uptime_events = self
            .state
            .pipe_debounced_guild_updates(self.debounced_guild_updates);

        // Emit the result of merging both streams
        uptime_events.merge(debounced_uptime_events)
    }
}

/// Shared tracker state that is used to coordinate state while a tracker runs
#[derive(Clone)]
struct TrackerState {
    config: Arc<Configuration>,
    active_guilds: DebouncedPool<u64>,
    connection_status: Arc<Mutex<ConnectionStatus>>,
}

assert_impl_all!(TrackerState: Sync, Send);

impl TrackerState {
    /// Stream processor that uses the stateful tracking information
    /// to generate the uptime events from the individual updates
    fn pipe_updates(
        &self,
        in_stream: impl Stream<Item = UpdateMessage>,
    ) -> impl Stream<Item = UptimeEvent> {
        let pool_copy = self.active_guilds.clone();
        let connection_status_mutex = Arc::clone(&self.connection_status);
        in_stream.flat_map(move |update| {
            match update {
                // For guild online/offline,
                // instead of emitting an event right now,
                // use the debounced pool and emit nothing
                UpdateMessage::GuildOnline(guild_id) => {
                    pool_copy.add(guild_id);
                    stream::iter(Vec::with_capacity(0))
                }
                UpdateMessage::GuildOffline(guild_id) => {
                    pool_copy.remove(guild_id);
                    stream::iter(Vec::with_capacity(0))
                }
                UpdateMessage::QueueOnline | UpdateMessage::GatewayOnline => {
                    let mut connection_status = connection_status_mutex
                        .lock()
                        .expect("connection status poisoned");
                    // Only emit an uptime event if the entire service just became online
                    let events = if connection_status.online_update(update) {
                        pool_copy.release();
                        let items = pool_copy.items::<Vec<_>>();
                        let events = vec![UptimeEvent::Online(items)];
                        events
                    } else {
                        Vec::with_capacity(0)
                    };
                    stream::iter(events)
                }
                UpdateMessage::QueueOffline | UpdateMessage::GatewayOffline => {
                    let mut connection_status = connection_status_mutex
                        .lock()
                        .expect("connection status poisoned");
                    // Only emit an uptime event if the entire service just became offline
                    let events = if connection_status.offline_update(update) {
                        let items = pool_copy.items::<Vec<_>>();
                        let events = vec![UptimeEvent::Offline(items)];
                        pool_copy.release();
                        events
                    } else {
                        Vec::with_capacity(0)
                    };
                    stream::iter(events)
                }
                UpdateMessage::GatewayHeartbeat => {
                    let connection_status = connection_status_mutex
                        .lock()
                        .expect("connection status poisoned");
                    let events = if connection_status.online() {
                        let mut events = if let Some(update) = pool_copy.release() {
                            UptimeEvent::from_pool_update(update)
                        } else {
                            Vec::new()
                        };
                        let items = pool_copy.items();
                        events.push(UptimeEvent::Heartbeat(items));
                        events
                    } else {
                        Vec::with_capacity(0)
                    };
                    stream::iter(events)
                }
            }
        })
    }

    /// Acts as a stream processor for the debounced bulk guild updates from the debounced pool,
    /// converting them into uptime events if the connection is online
    fn pipe_debounced_guild_updates(
        &self,
        in_stream: impl Stream<Item = DebouncedPoolUpdate<u64>>,
    ) -> impl Stream<Item = UptimeEvent> {
        let connection_status_mutex = Arc::clone(&self.connection_status);
        in_stream.flat_map(move |update| {
            let connection_status = connection_status_mutex
                .lock()
                .expect("connection status poisoned");
            let events = if connection_status.online() {
                UptimeEvent::from_pool_update(update)
            } else {
                Vec::with_capacity(0)
            };
            stream::iter(events)
        })
    }
}

/// Holds the connection state to the gateway and queue
struct ConnectionStatus {
    gateway_online: bool,
    queue_online: bool,
}

impl ConnectionStatus {
    fn new() -> Self {
        Self {
            gateway_online: true,
            queue_online: true,
        }
    }

    fn online(&self) -> bool {
        self.gateway_online && self.queue_online
    }

    fn online_update(&mut self, update: UpdateMessage) -> bool {
        let offline_before = !self.online();
        match update {
            UpdateMessage::QueueOnline => self.queue_online = true,
            UpdateMessage::GatewayOnline => self.gateway_online = true,
            _ => {}
        }
        let online_after = self.online();
        offline_before && online_after
    }

    fn offline_update(&mut self, update: UpdateMessage) -> bool {
        let online_before = self.online();
        match update {
            UpdateMessage::QueueOffline => self.queue_online = false,
            UpdateMessage::GatewayOffline => self.gateway_online = false,
            _ => {}
        }
        let offline_after = !self.online();
        online_before && offline_after
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Configuration;
    use crate::connection::{Tracker, UpdateMessage, UptimeEvent};
    use anyhow::Result;
    use futures::StreamExt;
    use std::collections::HashSet;
    use std::hash::Hash;
    use std::iter::FromIterator;
    use std::sync::Arc;
    use std::time::Duration;

    /// Defines set-equality for uptime events
    #[derive(Debug, Clone)]
    struct TestWrapper(UptimeEvent);
    impl PartialEq for TestWrapper {
        fn eq(&self, other: &Self) -> bool {
            match (&self.0, &other.0) {
                (UptimeEvent::Online(a), UptimeEvent::Online(b))
                | (UptimeEvent::Offline(a), UptimeEvent::Offline(b))
                | (UptimeEvent::Heartbeat(a), UptimeEvent::Heartbeat(b)) => set(a) == set(b),
                _ => false,
            }
        }
    }

    fn set<T: Hash + Eq + Clone>(v: &Vec<T>) -> HashSet<T> {
        HashSet::<T>::from_iter(v.iter().cloned())
    }

    #[tokio::test]
    async fn test_basic_debounced() -> Result<()> {
        let mut config = Configuration::default();
        config.guild_uptime_debounce_delay = Duration::from_millis(25);
        let (tracker, update_tx) = Tracker::new(Arc::new(config));
        let mut event_stream = tracker.stream_events();

        update_tx.send(UpdateMessage::GuildOnline(0))?;
        update_tx.send(UpdateMessage::GuildOnline(1))?;
        update_tx.send(UpdateMessage::GuildOnline(2))?;
        tokio::time::delay_for(Duration::from_millis(50)).await;
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Online(vec![0, 1, 2])))
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_heartbeat_flush() -> Result<()> {
        let mut config = Configuration::default();
        config.guild_uptime_debounce_delay = Duration::from_millis(25);
        let (tracker, update_tx) = Tracker::new(Arc::new(config));
        let mut event_stream = tracker.stream_events();

        update_tx.send(UpdateMessage::GuildOnline(0))?;
        update_tx.send(UpdateMessage::GuildOnline(1))?;
        tokio::time::delay_for(Duration::from_millis(50)).await;
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Online(vec![0, 1])))
        );

        update_tx.send(UpdateMessage::GuildOnline(2))?;
        update_tx.send(UpdateMessage::GuildOffline(0))?;
        update_tx.send(UpdateMessage::GatewayHeartbeat)?;
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Online(vec![2])))
        );
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Offline(vec![0])))
        );
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Heartbeat(vec![1, 2])))
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_offline_online() -> Result<()> {
        let mut config = Configuration::default();
        config.guild_uptime_debounce_delay = Duration::from_millis(25);
        let (tracker, update_tx) = Tracker::new(Arc::new(config));
        let mut event_stream = tracker.stream_events();

        update_tx.send(UpdateMessage::GuildOnline(0))?;
        update_tx.send(UpdateMessage::GuildOnline(1))?;
        tokio::time::delay_for(Duration::from_millis(50)).await;
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Online(vec![0, 1])))
        );

        update_tx.send(UpdateMessage::GatewayOffline)?;
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Offline(vec![0, 1])))
        );

        update_tx.send(UpdateMessage::QueueOffline)?;
        update_tx.send(UpdateMessage::QueueOnline)?;
        update_tx.send(UpdateMessage::GatewayOnline)?;
        assert_eq!(
            event_stream.next().await.map(TestWrapper),
            Some(TestWrapper(UptimeEvent::Online(vec![0, 1])))
        );

        Ok(())
    }
}
