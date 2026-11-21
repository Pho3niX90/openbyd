package io.github.pho3nix90.openbyd;

import com.fasterxml.jackson.annotation.JsonProperty;

/**
 * The state of the Rust runtime every client shares.
 *
 * @param workerThreads The runtime's worker threads.
 * @param started Whether the runtime has started: after the first client or call.
 */
public record RuntimeInfo(
        @JsonProperty("worker_threads") int workerThreads, @JsonProperty("started") boolean started) {}
