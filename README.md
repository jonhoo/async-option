# async-option

[![Crates.io](https://img.shields.io/crates/v/async-option.svg)](https://crates.io/crates/async-option)
[![Documentation](https://docs.rs/async-option/badge.svg)](https://docs.rs/async-option/)
[![Build Status](https://travis-ci.com/jonhoo/async-option.svg?branch=master)](https://travis-ci.com/jonhoo/async-option)
[![Codecov](https://codecov.io/github/jonhoo/async-option/coverage.svg?branch=master)](https://codecov.io/gh/jonhoo/async-option)

This crate provides an asynchronous, atomic `Option` type.

At a high level, this crate is exactly like `Arc<Mutex<Option<T>>>`, except with support for
asynchronous operations. Given an [`Aption<T>`], you can call [`poll_put`] to attempt to place
a value into the `Option`, or `poll_take` to take a value out of the `Option`. Both methods
will return `Async::NotReady` if the `Option` is occupied or empty respectively, and will at
that point have scheduled for the current task to be notified when the `poll_*` call may
succeed in the future. `Aption<T>` can also be used as a `Sink<SinkItem = T>` and `Stream<Item
= T>` by effectively operating as a single-element channel.

An `Aption<T>` can also be closed using [`poll_close`]. Any `poll_put` after a `poll_close`
will fail, and the next `poll_take` will return the current value (if any), and from then on
`poll_take` will return an error.

  [`Aption<T>`]: struct.Aption.html
  [`poll_put`]: struct.Aption.html#method.poll_put
  [`poll_take`]: struct.Aption.html#method.poll_take
  [`poll_close`]: struct.Aption.html#method.poll_close
