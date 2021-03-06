#[macro_use]
extern crate futures;
#[macro_use]
extern crate log;
extern crate indexmap;
#[cfg(test)]
extern crate quickcheck;
extern crate rand;
extern crate tokio_timer;
extern crate tower_discover;
extern crate tower_service;
extern crate tower_direct_service;

use futures::{Async, Future, Poll};
use indexmap::IndexMap;
use rand::{rngs::SmallRng, SeedableRng};
use std::{fmt, error};
use std::marker::PhantomData;
use tower_discover::Discover;
use tower_service::Service;
use tower_direct_service::DirectService;

pub mod choose;
pub mod load;

pub use choose::Choose;
pub use load::Load;

/// Balances requests across a set of inner services.
#[derive(Debug)]
pub struct Balance<D: Discover, C> {
    /// Provides endpoints from service discovery.
    discover: D,

    /// Determines which endpoint is ready to be used next.
    choose: C,

    /// Holds an index into `ready`, indicating the service that has been chosen to
    /// dispatch the next request.
    chosen_ready_index: Option<usize>,

    /// Holds an index into `ready`, indicating the service that dispatched the last
    /// request.
    dispatched_ready_index: Option<usize>,

    /// Holds all possibly-available endpoints (i.e. from `discover`).
    ready: IndexMap<D::Key, D::Service>,

    /// Newly-added endpoints that have not yet become ready.
    not_ready: IndexMap<D::Key, D::Service>,
}

/// Error produced by `Balance`
#[derive(Debug)]
pub enum Error<T, U> {
    Inner(T),
    Balance(U),
    NotReady,
}

pub struct ResponseFuture<F: Future, E>(F, PhantomData<E>);

// ===== impl Balance =====

impl<D> Balance<D, choose::PowerOfTwoChoices>
where
    D: Discover,
    D::Service: Load,
    <D::Service as Load>::Metric: PartialOrd + fmt::Debug,
{
    /// Chooses services using the [Power of Two Choices][p2c].
    ///
    /// This configuration is prefered when a load metric is known.
    ///
    /// As described in the [Finagle Guide][finagle]:
    ///
    /// > The algorithm randomly picks two services from the set of ready endpoints and
    /// > selects the least loaded of the two. By repeatedly using this strategy, we can
    /// > expect a manageable upper bound on the maximum load of any server.
    /// >
    /// > The maximum load variance between any two servers is bound by `ln(ln(n))` where
    /// > `n` is the number of servers in the cluster.
    ///
    /// [finagle]: https://twitter.github.io/finagle/guide/Clients.html#power-of-two-choices-p2c-least-loaded
    /// [p2c]: http://www.eecs.harvard.edu/~michaelm/postscripts/handbook2001.pdf
    pub fn p2c(discover: D) -> Self {
        Self::new(discover, choose::PowerOfTwoChoices::default())
    }

    /// Initializes a P2C load balancer from the provided randomization source.
    ///
    /// This may be preferable when an application instantiates many balancers.
    pub fn p2c_with_rng<R: rand::Rng>(
        discover: D,
        rng: &mut R,
    ) -> Result<Self, rand::Error> {
        let rng = SmallRng::from_rng(rng)?;
        Ok(Self::new(discover, choose::PowerOfTwoChoices::new(rng)))
    }
}

impl<D: Discover> Balance<D, choose::RoundRobin> {
    /// Attempts to choose services sequentially.
    ///
    /// This configuration is prefered when no load metric is known.
    pub fn round_robin(discover: D) -> Self {
        Self::new(discover, choose::RoundRobin::default())
    }
}

impl<D, C> Balance<D, C>
where
    D: Discover,
    C: Choose<D::Key, D::Service>,
{
    /// Creates a new balancer.
    pub fn new(discover: D, choose: C) -> Self {
        Self {
            discover,
            choose,
            chosen_ready_index: None,
            dispatched_ready_index: None,
            ready: IndexMap::default(),
            not_ready: IndexMap::default(),
        }
    }

    /// Returns true iff there are ready services.
    ///
    /// This is not authoritative and is only useful after `poll_ready` has been called.
    pub fn is_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Returns true iff there are no ready services.
    ///
    /// This is not authoritative and is only useful after `poll_ready` has been called.
    pub fn is_not_ready(&self) -> bool {
        self.ready.is_empty()
    }

    /// Counts the number of services considered to be ready.
    ///
    /// This is not authoritative and is only useful after `poll_ready` has been called.
    pub fn num_ready(&self) -> usize {
        self.ready.len()
    }

    /// Counts the number of services not considered to be ready.
    ///
    /// This is not authoritative and is only useful after `poll_ready` has been called.
    pub fn num_not_ready(&self) -> usize {
        self.not_ready.len()
    }

    /// Polls `discover` for updates, adding new items to `not_ready`.
    ///
    /// Removals may alter the order of either `ready` or `not_ready`.
    fn update_from_discover<E>(&mut self) -> Result<(), Error<E, D::Error>> {
        debug!("updating from discover");
        use tower_discover::Change::*;

        while let Async::Ready(change) = self.discover.poll().map_err(Error::Balance)? {
            match change {
                Insert(key, mut svc) => {
                    // If the `Insert`ed service is a duplicate of a service already
                    // in the ready list, remove the ready service first. The new
                    // service will then be inserted into the not-ready list.
                    self.ready.remove(&key);

                    self.not_ready.insert(key, svc);
                }

                Remove(key) => {
                    let _ejected = match self.ready.remove(&key) {
                        None => self.not_ready.remove(&key),
                        Some(s) => Some(s),
                    };
                    // XXX is it safe to just drop the Service? Or do we need some sort of
                    // graceful teardown?
                    // TODO: poll_close
                }
            }
        }

        Ok(())
    }

    /// Calls `poll_ready` on all services in `not_ready`.
    ///
    /// When `poll_ready` returns ready, the service is removed from `not_ready` and inserted
    /// into `ready`, potentially altering the order of `ready` and/or `not_ready`.
    fn promote_to_ready<F, E>(&mut self, mut poll_ready: F) -> Result<(), Error<E, D::Error>>
    where
        F: FnMut(&mut D::Service) -> Poll<(), E>,
    {
        let n = self.not_ready.len();
        if n == 0 {
            trace!("promoting to ready: not_ready is empty, skipping.");
            return Ok(());
        }

        debug!("promoting to ready: {}", n);
        // Iterate through the not-ready endpoints from right to left to prevent removals
        // from reordering services in a way that could prevent a service from being polled.
        for idx in (0..n).rev() {
            let is_ready = {
                let (_, svc) = self.not_ready
                    .get_index_mut(idx)
                    .expect("invalid not_ready index");;
                poll_ready(svc).map_err(Error::Inner)?.is_ready()
            };
            trace!("not_ready[{:?}]: is_ready={:?};", idx, is_ready);
            if is_ready {
                debug!("not_ready[{:?}]: promoting to ready", idx);
                let (key, svc) = self.not_ready
                    .swap_remove_index(idx)
                    .expect("invalid not_ready index");
                self.ready.insert(key, svc);
            } else {
                debug!("not_ready[{:?}]: not promoting to ready", idx);
            }
        }

        debug!("promoting to ready: done");

        Ok(())
    }

    /// Polls a `ready` service or moves it to `not_ready`.
    ///
    /// If the service exists in `ready` and does not poll as ready, it is moved to
    /// `not_ready`, potentially altering the order of `ready` and/or `not_ready`.
    fn poll_ready_index<F, E>(
        &mut self,
        idx: usize,
        mut poll_ready: F,
    ) -> Option<Poll<(), Error<E, D::Error>>>
    where
        F: FnMut(&mut D::Service) -> Poll<(), E>,
    {
        match self.ready.get_index_mut(idx) {
            None => return None,
            Some((_, svc)) => {
                match poll_ready(svc) {
                    Ok(Async::Ready(())) => return Some(Ok(Async::Ready(()))),
                    Err(e) => return Some(Err(Error::Inner(e))),
                    Ok(Async::NotReady) => {}
                }
            }
        }

        let (key, svc) = self.ready.swap_remove_index(idx).expect("invalid ready index");
        self.not_ready.insert(key, svc);
        Some(Ok(Async::NotReady))
    }

    /// Chooses the next service to which a request will be dispatched.
    ///
    /// Ensures that .
    fn choose_and_poll_ready<F, E>(&mut self, mut poll_ready: F) -> Poll<(), Error<E, D::Error>>
    where
        F: FnMut(&mut D::Service) -> Poll<(), E>,
    {
        loop {
            let n = self.ready.len();
            debug!("choosing from {} replicas", n);
            let idx = match n {
                0 => return Ok(Async::NotReady),
                1 => 0,
                _ => {
                    let replicas = choose::replicas(&self.ready).expect("too few replicas");
                    self.choose.choose(replicas)
                }
            };

            // XXX Should we handle per-endpoint errors?
            if self
                .poll_ready_index(idx, &mut poll_ready)
                .expect("invalid ready index")?
                .is_ready()
            {
                self.chosen_ready_index = Some(idx);
                return Ok(Async::Ready(()));
            }
        }
    }

    fn poll_ready_inner<F, E>(&mut self, mut poll_ready: F) -> Poll<(), Error<E, D::Error>>
    where
        F: FnMut(&mut D::Service) -> Poll<(), E>,
    {
        // Clear before `ready` is altered.
        self.chosen_ready_index = None;

        // Before `ready` is altered, check the readiness of the last-used service, moving it
        // to `not_ready` if appropriate.
        if let Some(idx) = self.dispatched_ready_index.take() {
            // XXX Should we handle per-endpoint errors?
            self.poll_ready_index(idx, &mut poll_ready)
                .expect("invalid dispatched ready key")?;
        }

        // Update `not_ready` and `ready`.
        self.update_from_discover()?;
        self.promote_to_ready(&mut poll_ready)?;

        // Choose the next service to be used by `call`.
        self.choose_and_poll_ready(&mut poll_ready)
    }

    fn call<Request, F, FF>(&mut self, call: F, request: Request) -> ResponseFuture<FF, D::Error>
    where
        F: FnOnce(&mut D::Service, Request) -> FF,
        FF: Future,
    {
        let idx = self.chosen_ready_index.take().expect("not ready");
        let (_, svc) = self.ready.get_index_mut(idx).expect("invalid chosen ready index");
        self.dispatched_ready_index = Some(idx);

        let rsp = call(svc, request);
        ResponseFuture(rsp, PhantomData)
    }
}

impl<D, C, Request> Service<Request> for Balance<D, C>
where
    D: Discover,
    D::Service: Service<Request>,
    C: Choose<D::Key, D::Service>,
{
    type Response = <D::Service as Service<Request>>::Response;
    type Error = Error<<D::Service as Service<Request>>::Error, D::Error>;
    type Future = ResponseFuture<<D::Service as Service<Request>>::Future, D::Error>;

    /// Prepares the balancer to process a request.
    ///
    /// When `Async::Ready` is returned, `chosen_ready_index` is set with a valid index
    /// into `ready` referring to a `Service` that is ready to disptach a request.
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.poll_ready_inner(D::Service::poll_ready)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        self.call(D::Service::call, request)
    }
}

impl<D, C, Request> DirectService<Request> for Balance<D, C>
where
    D: Discover,
    D::Service: DirectService<Request>,
    C: Choose<D::Key, D::Service>,
{
    type Response = <D::Service as DirectService<Request>>::Response;
    type Error = Error<<D::Service as DirectService<Request>>::Error, D::Error>;
    type Future = ResponseFuture<<D::Service as DirectService<Request>>::Future, D::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.poll_ready_inner(D::Service::poll_ready)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        self.call(D::Service::call, request)
    }

    fn poll_service(&mut self) -> Poll<(), Self::Error> {
        let mut any_not_ready = false;

        // TODO: don't re-poll services that return Ready until call is invoked on them

        for (_, svc) in &mut self.ready {
            if let Async::NotReady = svc.poll_service().map_err(Error::Inner)? {
                any_not_ready = true;
            }
        }

        for (_, svc) in &mut self.not_ready {
            if let Async::NotReady = svc.poll_service().map_err(Error::Inner)? {
                any_not_ready = true;
            }
        }

        if any_not_ready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }

    fn poll_close(&mut self) -> Poll<(), Self::Error> {
        let mut err = None;
        self.ready.retain(|_, svc| match svc.poll_close() {
            Ok(Async::Ready(())) => return false,
            Ok(Async::NotReady) => return true,
            Err(e) => {
                err = Some(e);
                return false;
            }
        });
        self.not_ready.retain(|_, svc| match svc.poll_close() {
            Ok(Async::Ready(())) => return false,
            Ok(Async::NotReady) => return true,
            Err(e) => {
                err = Some(e);
                return false;
            }
        });

        if let Some(e) = err {
            return Err(Error::Inner(e));
        }

        if self.ready.is_empty() && self.not_ready.is_empty() {
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }
}

// ===== impl ResponseFuture =====

impl<F: Future, E> Future for ResponseFuture<F, E> {
    type Item = F::Item;
    type Error = Error<F::Error, E>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll().map_err(Error::Inner)
    }
}


// ===== impl Error =====

impl<T, U> fmt::Display for Error<T, U>
where
    T: fmt::Display,
    U: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Inner(ref why) => fmt::Display::fmt(why, f),
            Error::Balance(ref why) =>
                write!(f, "load balancing failed: {}", why),
            Error::NotReady => f.pad("not ready"),
        }
    }
}

impl<T, U> error::Error for Error<T, U>
where
    T: error::Error,
    U: error::Error,
{
    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::Inner(ref why) => Some(why),
            Error::Balance(ref why) => Some(why),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Inner(_) => "inner service error",
            Error::Balance(_) => "load balancing failed",
            Error::NotReady => "not ready",
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future;
    use quickcheck::*;
    use std::collections::VecDeque;
    use tower_discover::Change;

    use super::*;

    struct ReluctantDisco(VecDeque<Change<usize, ReluctantService>>);

    struct ReluctantService {
        polls_until_ready: usize,
    }

    impl Discover for ReluctantDisco {
        type Key = usize;
        type Service = ReluctantService;
        type Error = ();

        fn poll(&mut self) -> Poll<Change<Self::Key, Self::Service>, Self::Error> {
            let r = self.0.pop_front().map(Async::Ready).unwrap_or(Async::NotReady);
            debug!("polling disco: {:?}", r.is_ready());
            Ok(r)
        }
    }

    impl Service<()> for ReluctantService {
        type Response = ();
        type Error = ();
        type Future = future::FutureResult<(), ()>;

        fn poll_ready(&mut self) -> Poll<(), ()> {
            if self.polls_until_ready == 0 {
                return Ok(Async::Ready(()));
            }

            self.polls_until_ready -= 1;
            return Ok(Async::NotReady);
        }

        fn call(&mut self, _: ()) -> Self::Future {
            future::ok(())
        }
    }

    quickcheck! {
        /// Creates a random number of services, each of which must be polled a random
        /// number of times before becoming ready. As the balancer is polled, ensure that
        /// it does not become ready prematurely and that services are promoted from
        /// not_ready to ready.
        fn poll_ready(service_tries: Vec<usize>) -> TestResult {
            // Stores the number of pending services after each poll_ready call.
            let mut pending_at = Vec::new();

            let disco = {
                let mut changes = VecDeque::new();
                for (i, n) in service_tries.iter().map(|n| *n).enumerate() {
                    for j in 0..n {
                        if j == pending_at.len() {
                            pending_at.push(1);
                        } else {
                            pending_at[j] += 1;
                        }
                    }

                    let s = ReluctantService { polls_until_ready: n };
                    changes.push_back(Change::Insert(i, s));
                }
                ReluctantDisco(changes)
            };
            pending_at.push(0);

            let mut balancer = Balance::new(disco, choose::RoundRobin::default());

            let services = service_tries.len();
            let mut next_pos = 0;
            for pending in pending_at.iter().map(|p| *p) {
                assert!(pending <= services);
                let ready = services - pending;

                match balancer.poll_ready() {
                    Err(_) => return TestResult::error("poll_ready failed"),
                    Ok(p) => {
                        if p.is_ready() != (ready > 0) {
                            return TestResult::failed();
                        }
                    }
                }

                if balancer.num_ready() != ready {
                    return TestResult::failed();
                }

                if balancer.num_not_ready() != pending {
                    return TestResult::failed();
                }

                if balancer.is_ready() != (ready > 0) {
                    return TestResult::failed();
                }
                if balancer.is_not_ready() != (ready == 0) {
                    return TestResult::failed();
                }

                if balancer.dispatched_ready_index.is_some() {
                    return TestResult::failed();
                }

                if ready == 0 {
                    if balancer.chosen_ready_index.is_some() {
                        return TestResult::failed();
                    }
                } else {
                    // Check that the round-robin chooser is doing its thing:
                    match balancer.chosen_ready_index {
                        None => return TestResult::failed(),
                        Some(idx) => {
                            if idx != next_pos  {
                                return TestResult::failed();
                            }
                        }
                    }

                    next_pos = (next_pos + 1) % ready;
                }
            }

            TestResult::passed()
        }
    }
}
