use futures::{
    prelude::*, 
    executor::{self as exec,ThreadPool, ThreadPoolBuilder},
    channel::mpsc, 
    stream::{self,FuturesUnordered}
};
use std::{
    fmt,
    pin::Pin, 
    task::{ Context,Poll}
};
use fnv::FnvHashMap;


#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(pub usize);

#[derive(Debug)]
pub enum Command<T> {
    /// Notify the connection handler of an event.
    NotifyHandler(T),
}


/// Events that a task can emit to its manager.
#[derive(Debug)]
pub enum Event {
    /// A connection to a node has succeeded.
    Established { id: TaskId },
    /// An established connection produced an error.
    Error { id: TaskId },
    /// A pending connection failed.
    Failed { id: TaskId },
    /// Notify the manager of an event from the connection.
    Notify { id: TaskId }
}



/// The state associated with the `Task` of a connection.

impl Event {
    pub fn id(&self) -> &TaskId {
        match self {
            Event::Established { id } => id,
            Event::Error { id } => id,
            Event::Notify { id } => id,
            Event::Failed { id } => id,
        }
    }
}

pub struct Task<I, F>
{
    /// The ID of this task.
    id: TaskId,

    /// Sender to emit events to the manager of this task.
    events: mpsc::Sender<Event>,

    /// Receiver for commands sent by the manager of this task.
    commands: stream::Fuse<mpsc::Receiver<Command<I>>>,

    /// Inner state of this `Task`.
    state: State<F>,
}

impl<I, F> Task<I,F>
{
    pub fn pending(
        id: TaskId,
        events: mpsc::Sender<Event>,
        commands: mpsc::Receiver<Command<I>>,
        future: F,
    ) -> Self {
        Task {
            id,
            events,
            commands: commands.fuse(),
            state: State::Pending {
                future: Box::pin(future),
            },
        }
    }
}


enum State<F>
{   
    Pending {
        /// The intended handler for the established connection.
        future:Pin<Box<F>>
    },
    Ready {
        /// The actual event message to send.
        event: Event
    },
    /// The task has finished.
    Done
}

impl<I, F> Unpin for Task<I, F>
{
}


impl<I, F> Future for Task<I, F>
{
    type Output = ();

    // NOTE: It is imperative to always consume all incoming commands from
    // the manager first, in order to not prevent it from making progress because
    // it is blocked on the channel capacity.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        let this = &mut *self;
        let id = this.id;

        'poll: loop {
            match std::mem::replace(&mut this.state, State::Done) {
                State::Pending { mut future } => {
                    // Check if the manager aborted this task by dropping the `commands`
                    // channel sender side.
                    match Stream::poll_next(Pin::new(&mut this.commands), cx) {
                        Poll::Pending => {},
                        Poll::Ready(None) => return Poll::Ready(()),
                        Poll::Ready(Some(Command::NotifyHandler(_))) => unreachable!(
                            "Manager does not allow sending commands to pending tasks.",
                        )
                    }
                }
                State::Ready { event } => {
                    // Process commands received from the manager, if any.
                    loop {
                        match Stream::poll_next(Pin::new(&mut this.commands), cx) {
                            Poll::Pending => break,
                            Poll::Ready(Some(Command::NotifyHandler(event))) =>break,
                            Poll::Ready(None) =>
                                // The manager has dropped the task, thus initiate a
                                // graceful shutdown of the connection, if given.
                                // if let Some(c) = connection {
                                //     this.state = State::Closing(c.close());
                                //    continue 'poll
                                // } else {
                                //     return Poll::Ready(())
                                // }
                                return Poll::Ready(())
                        }
                    }
                    // Send the event to the manager.
                    match this.events.poll_ready(cx) {
                        Poll::Pending => {
                            self.state = State::Ready { event };
                            return Poll::Pending
                        }
                        Poll::Ready(Ok(())) => {
                            // We assume that if `poll_ready` has succeeded, then sending the event
                            // will succeed as well. If it turns out that it didn't, we will detect
                            // the closing at the next loop iteration.
                            let _ = this.events.start_send(event);
                        },
                        Poll::Ready(Err(_)) => {
                            // The manager is no longer reachable, maybe due to
                            // application shutdown. Try a graceful shutdown of the
                            // connection, if available, and end the task.
                            return Poll::Ready(())
                        }
                    }
                }

                State::Done => panic!("`Task::poll()` called after completion.")
            }
        }
    }
}

pub trait Executor {
    /// Run the given future in the background until it ends.
    fn exec(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>);
}

impl<'a, T: ?Sized + Executor> Executor for &'a T {
    fn exec(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) {
        T::exec(&**self, f)
    }
}

impl<'a, T: ?Sized + Executor> Executor for &'a mut T {
    fn exec(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) {
        T::exec(&**self, f)
    }
}

impl<T: ?Sized + Executor> Executor for Box<T> {
    fn exec(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) {
        T::exec(&**self, f)
    }
}


/// A connection `Manager` orchestrates the I/O of a set of connections.
pub struct Manager<I> {
    /// The tasks of the managed connections.
    ///
    /// Each managed connection is associated with a (background) task
    /// spawned onto an executor. Each `TaskInfo` in `tasks` is linked to such a
    /// background task via a channel. Closing that channel (i.e. dropping
    /// the sender in the associated `TaskInfo`) stops the background task,
    /// which will attempt to gracefully close the connection.
    tasks: FnvHashMap<TaskId, TaskInfo<I>>,

    /// Next available identifier for a new connection / task.
    next_task_id: TaskId,

    /// The executor to use for running the background tasks. If `None`,
    /// the tasks are kept in `local_spawns` instead and polled on the
    /// current thread when the manager is polled for new events.
    executor: Option<Box<dyn Executor + Send>>,

    /// If no `executor` is configured, tasks are kept in this set and
    /// polled on the current thread when the manager is polled for new events.
    local_spawns: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>>,

    /// Sender distributed to managed tasks for reporting events back
    /// to the manager.
    events_tx: mpsc::Sender<Event>,

    /// Receiver for events reported from managed tasks.
    events_rx: mpsc::Receiver<Event>
}

impl<I> fmt::Debug for Manager<I>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_map()
            .entries(self.tasks.iter().map(|(id, task)| (id, &task.state)))
            .finish()
    }
}

impl<I> Manager<I> {
    /// Creates a new connection manager.
    pub fn new(executor: Option<Box<dyn Executor + Send>>) -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            tasks: FnvHashMap::default(),
            next_task_id: TaskId(0),
            executor,
            local_spawns: FuturesUnordered::new(),
            events_tx: tx,
            events_rx: rx
        }
    }

    /// Adds to the manager a future that tries to reach a node.
    ///
    /// This method spawns a task dedicated to resolving this future and
    /// processing the node's events.
    pub fn add_pending<F>(&mut self, future: F)
    where 
    I: Send + 'static,
    F: Future<Output = ()> + Send + 'static,
    {
        let task_id = self.next_task_id;
        self.next_task_id.0 += 1;

        let (tx, rx) = mpsc::channel(4);
        self.tasks.insert(task_id, TaskInfo { sender: tx, state: TaskState::Pending });

        let task = Box::pin(Task::<I, F>::pending(task_id, self.events_tx.clone(), rx, future));
        if let Some(executor) = &mut self.executor {
            print!("{}",1.to_string());
            executor.exec(task);
        } 
        // else {
        //     print!("{}",2.to_string());
        //     self.local_spawns.push(task);
        // }
    }
}



#[derive(Debug)]
struct TaskInfo<I> {
    /// Channel endpoint to send messages to the task.
    sender: mpsc::Sender<Command<I>>,
    /// The state of the task as seen by the `Manager`.
    state: TaskState,
}

/// Internal state of a running task as seen by the `Manager`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskState {
    /// The connection is being established.
    Pending,
    /// The connection is established.
    Established,
}




fn main() {
    // println!("Hello, world!");
    struct PoolWrapper(ThreadPool);
    impl Executor for PoolWrapper {
    fn exec(&self, f: Pin<Box<dyn Future<Output = ()> + Send>>) {
                    self.0.spawn_ok(f)
    }
    }

    match ThreadPoolBuilder::new()
    .name_prefix("task-")
    .create()
    .map(|tp| Box::new(PoolWrapper(tp)) as Box<_>)
    {
    Ok(executor) => { 
       let mut manager = Manager::<usize>::new(Some(Box::new(executor)));
    //    manager.add_pending(async move {
    //     println!("Hello from task");
    // });
    exec::block_on(
        async move {
            println!("Hello from task");
        }
    );
    },
    Err(err) => {
        
    }
    }
}
