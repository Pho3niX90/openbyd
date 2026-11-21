/**
 * openbyd: an unofficial client for the BYD overseas app API, with a Rust core.
 *
 * <ul>
 *   <li>{@link io.github.pho3nix90.openbyd.Client}: pyBYD's {@code BydClient}.
 *   <li>{@link io.github.pho3nix90.openbyd.Car}: pyBYD's {@code BydCar}, from {@code
 *       client.getCar(vin)}.
 *   <li>pyBYD's exception hierarchy under {@link io.github.pho3nix90.openbyd.OpenBydException}.
 *   <li>{@link io.github.pho3nix90.openbyd.OpenByd}: what needs no client, such as the envelope
 *       cipher and parsing a raw payload as one of pyBYD's models.
 *   <li>{@link io.github.pho3nix90.openbyd.model}: pyBYD's models and enums, and what a caller
 *       builds.
 * </ul>
 *
 * <p>Nothing here is null unless its type is marked {@code @Nullable}.
 *
 * <p>This project is not affiliated with, endorsed by or supported by BYD.
 */
@NullMarked
package io.github.pho3nix90.openbyd;

import org.jspecify.annotations.NullMarked;
