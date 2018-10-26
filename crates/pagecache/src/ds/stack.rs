// lock-free stack
use std::{
    fmt::{self, Debug},
    ops::Deref,
    sync::atomic::Ordering::{Relaxed, SeqCst},
};

use crate::{
    debug_delay,
    epoch::{pin, unprotected, Atomic, Guard, Owned, Shared},
};

/// A node in the lock-free `Stack`.
#[derive(Debug)]
pub(crate) struct Node<T: Send + 'static> {
    inner: T,
    next: Atomic<Node<T>>,
}

impl<T: Send + 'static> Drop for Node<T> {
    fn drop(&mut self) {
        unsafe {
            let next =
                self.next.load(Relaxed, unprotected()).as_raw();
            if !next.is_null() {
                drop(Box::from_raw(next as *mut Node<T>));
            }
        }
    }
}

/// A simple lock-free stack, with the ability to atomically
/// append or entirely swap-out entries.
pub(crate) struct Stack<T: Send + 'static> {
    head: Atomic<Node<T>>,
}

impl<T: Send + 'static> Default for Stack<T> {
    fn default() -> Stack<T> {
        Stack {
            head: Atomic::null(),
        }
    }
}

impl<T: Send + 'static> Drop for Stack<T> {
    fn drop(&mut self) {
        unsafe {
            let curr =
                self.head.load(Relaxed, unprotected()).as_raw();
            if !curr.is_null() {
                drop(Box::from_raw(curr as *mut Node<T>));
            }
        }
    }
}

impl<T> Debug for Stack<T>
where
    T: Debug + Send + 'static + Sync,
{
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> Result<(), fmt::Error> {
        let guard = pin();
        let head = self.head(&guard);
        let iter = StackIter::from_ptr(head, &guard);

        formatter.write_str("Stack [")?;
        let mut written = false;
        for node in iter {
            if written {
                formatter.write_str(", ")?;
            }
            formatter
                .write_str(&*format!("({:?}) ", &node as *const _))?;
            node.fmt(formatter)?;
            written = true;
        }
        formatter.write_str("]")?;
        Ok(())
    }
}

impl<T: Send + 'static> Deref for Node<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: Send + Sync + 'static> Stack<T> {
    /// Add an item to the stack, spinning until successful.
    pub(crate) fn push(&self, inner: T) {
        debug_delay();
        let node = Owned::new(Node {
            inner: inner,
            next: Atomic::null(),
        });

        unsafe {
            let node = node.into_shared(unprotected());

            loop {
                let head = self.head(unprotected());
                node.deref().next.store(head, SeqCst);
                if self
                    .head
                    .compare_and_set(
                        head,
                        node,
                        SeqCst,
                        unprotected(),
                    ).is_ok()
                {
                    return;
                }
            }
        }
    }

    /// Pop the next item off the stack. Returns None if nothing is there.
    fn _pop<'g>(&self, guard: &'g Guard) -> Option<T> {
        use std::ptr;
        debug_delay();
        let mut head = self.head(&guard);
        loop {
            match unsafe { head.as_ref() } {
                Some(h) => {
                    let next = h.next.load(SeqCst, &guard);
                    match self
                        .head
                        .compare_and_set(head, next, SeqCst, &guard)
                    {
                        Ok(_) => unsafe {
                            let head_owned = head.into_owned();
                            guard.defer(move || head_owned);
                            return Some(ptr::read(&h.inner));
                        },
                        Err(h) => head = h.current,
                    }
                }
                None => return None,
            }
        }
    }

    /// compare and push
    pub(crate) fn cap<'g>(
        &self,
        old: Shared<'_, Node<T>>,
        new: T,
        guard: &'g Guard,
    ) -> Result<Shared<'g, Node<T>>, Shared<'g, Node<T>>> {
        debug_delay();
        let node = Owned::new(Node {
            inner: new,
            next: Atomic::from(old),
        });

        let node = node.into_shared(guard);

        let res = self.head.compare_and_set(old, node, SeqCst, guard);

        match res {
            Err(e) => {
                unsafe {
                    // we want to set next to null to prevent
                    // the current shared head from being
                    // dropped when we drop this node.
                    node.deref().next.store(Shared::null(), SeqCst);
                    let node_owned = node.into_owned();
                    guard.defer(move || node_owned);
                }
                Err(e.current)
            }
            Ok(_) => Ok(node),
        }
    }

    /// compare and swap
    pub(crate) fn cas<'g>(
        &self,
        old: Shared<'g, Node<T>>,
        new: Shared<'g, Node<T>>,
        guard: &'g Guard,
    ) -> Result<Shared<'g, Node<T>>, Shared<'g, Node<T>>> {
        debug_delay();
        let res = self.head.compare_and_set(old, new, SeqCst, guard);

        match res {
            Ok(_) => {
                if !old.is_null() {
                    unsafe {
                        let old_owned = old.into_owned();
                        guard.defer(move || old_owned)
                    };
                }
                Ok(new)
            }
            Err(e) => {
                if !new.is_null() {
                    unsafe {
                        let new_owned = new.into_owned();
                        guard.defer(move || new_owned)
                    };
                }

                Err(e.current)
            }
        }
    }

    /// Returns the current head pointer of the stack, which can
    /// later be used as the key for cas and cap operations.
    pub(crate) fn head<'g>(
        &self,
        guard: &'g Guard,
    ) -> Shared<'g, Node<T>> {
        self.head.load(SeqCst, guard)
    }
}

/// An iterator over nodes in a lock-free stack.
pub(crate) struct StackIter<'a, T>
where
    T: 'a + Send + 'static + Sync,
{
    inner: Shared<'a, Node<T>>,
    guard: &'a Guard,
}

impl<'a, T> StackIter<'a, T>
where
    T: 'a + Send + 'static + Sync,
{
    /// Creates a StackIter from a pointer to one.
    pub(crate) fn from_ptr<'b>(
        ptr: Shared<'b, Node<T>>,
        guard: &'b Guard,
    ) -> StackIter<'b, T> {
        StackIter {
            inner: ptr,
            guard: guard,
        }
    }
}

impl<'a, T> Iterator for StackIter<'a, T>
where
    T: Send + 'static + Sync,
{
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        debug_delay();
        if self.inner.is_null() {
            None
        } else {
            unsafe {
                let ret = &self.inner.deref().inner;
                self.inner =
                    self.inner.deref().next.load(SeqCst, self.guard);
                Some(ret)
            }
        }
    }
}

/// Turns a vector of elements into a lock-free stack
/// of them, and returns the head of the stack.
pub(crate) fn node_from_frag_vec<T>(from: Vec<T>) -> Owned<Node<T>>
where
    T: Send + 'static + Sync,
{
    let mut last = None;

    for item in from.into_iter().rev() {
        last = if let Some(last) = last {
            Some(Owned::new(Node {
                inner: item,
                next: Atomic::from(last),
            }))
        } else {
            Some(Owned::new(Node {
                inner: item,
                next: Atomic::null(),
            }))
        }
    }

    last.expect("at least one frag was provided in the from Vec")
}

#[test]
fn basic_functionality() {
    use std::sync::Arc;
    use std::thread;

    let guard = pin();
    let ll = Arc::new(Stack::default());
    assert_eq!(ll._pop(&guard), None);
    ll.push(1);
    let ll2 = Arc::clone(&ll);
    let t = thread::spawn(move || {
        ll2.push(2);
        ll2.push(3);
        ll2.push(4);
    });
    t.join().unwrap();
    ll.push(5);
    assert_eq!(ll._pop(&guard), Some(5));
    assert_eq!(ll._pop(&guard), Some(4));
    let ll3 = Arc::clone(&ll);
    let t = thread::spawn(move || {
        let guard = pin();
        assert_eq!(ll3._pop(&guard), Some(3));
        assert_eq!(ll3._pop(&guard), Some(2));
    });
    t.join().unwrap();
    assert_eq!(ll._pop(&guard), Some(1));
    let ll4 = Arc::clone(&ll);
    let t = thread::spawn(move || {
        let guard = pin();
        assert_eq!(ll4._pop(&guard), None);
    });
    t.join().unwrap();
}
