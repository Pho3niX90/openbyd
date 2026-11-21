package io.github.pho3nix90.openbyd;

/**
 * One vehicle: pyBYD's {@code BydCar}. Get one from {@link Client#getCar(String)}.
 *
 * <p>It holds the vehicle's merged state and offers its commands by feature:
 *
 * <pre>{@code
 * Car car = client.getCar(vin).get();
 * car.updateRealtime().get();
 * car.lock().lock().get();
 * car.hvac().start(new ClimateOptions().temperature(21).duration(20)).get();
 * car.state().realtime().leftFrontDoorLock(); // LockState.LOCKED, projected
 * }</pre>
 *
 * <p>A command lays the state it expects over the current state at once (a projection) and rolls
 * it back when it fails; the next push or update that agrees confirms it. Each feature tells
 * whether the car supports it; a command the car does not support fails with {@link
 * BydEndpointNotSupportedException} without sending anything. A car whose client is closed fails
 * its calls with {@link BydException}.
 */
public final class Car extends CarMethods {
    private final Link link;
    private final String vin;

    Car(Link link, String vin) {
        super(link);
        this.link = link;
        this.vin = vin;
    }

    /** The vehicle identification number. */
    public String vin() {
        return vin;
    }

    /**
     * Release the car's resources (nothing yet; pyBYD's lifecycle hook). A car is closed with
     * its client, so closing it afterwards does nothing.
     */
    @Override
    public void close() {
        try {
            super.close();
        } catch (BydException error) {
            if (!link.core().closed()) {
                throw error;
            }
        }
    }
}
