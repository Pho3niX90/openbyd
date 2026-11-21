package io.github.pho3nix90.openbyd;

import io.github.pho3nix90.openbyd.model.VehicleSnapshot;
import java.util.Map;
import java.util.Objects;
import java.util.function.BiConsumer;
import org.jspecify.annotations.Nullable;

/** The options of {@link Client#getCar(String, GetCarOptions)}. An option that is not set takes its default. */
public final class GetCarOptions {
    @Nullable Map<String, ? extends @Nullable Object> vehicle;
    @Nullable Double projectionTtl;
    @Nullable BiConsumer<String, VehicleSnapshot> onStateChanged;

    /** Options with every value left to its default. */
    public GetCarOptions() {}

    /**
     * The vehicle's raw payload, as the vehicle list gives it; the vehicle list is fetched when
     * not set.
     *
     * @return this, to chain
     */
    public GetCarOptions vehicle(Map<String, ? extends @Nullable Object> vehicle) {
        this.vehicle = Objects.requireNonNull(vehicle, "vehicle");
        return this;
    }

    /**
     * How long a command's projections stay active, in seconds. Default {@code 30}.
     *
     * @return this, to chain
     */
    public GetCarOptions projectionTtl(double seconds) {
        this.projectionTtl = seconds;
        return this;
    }

    /**
     * Hears every change of the car's state: the VIN and the new state. It runs on the client's
     * callback thread.
     *
     * @return this, to chain
     */
    public GetCarOptions onStateChanged(BiConsumer<String, VehicleSnapshot> listener) {
        this.onStateChanged = Objects.requireNonNull(listener, "listener");
        return this;
    }
}
