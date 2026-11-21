namespace OpenByd;

/// <summary>
/// One vehicle: pyBYD's <c>BydCar</c>. Get one from <see cref="Client.GetCarAsync"/>.
/// </summary>
/// <remarks>
/// <para>
/// It holds the vehicle's merged state (<see cref="State"/>) and offers its commands by
/// feature: <see cref="Lock"/>, <see cref="Hvac"/>, <see cref="Seat"/> and the others.
/// </para>
/// <para>
/// A command lays the state it expects over the current state at once (a projection) and
/// rolls it back when it fails; the next push or update that agrees confirms it. Each
/// feature tells whether the car supports it; a command the car does not support fails
/// with <see cref="BydEndpointNotSupportedException"/> without sending anything. A car
/// whose client is closed fails its calls with <see cref="BydException"/>.
/// </para>
/// </remarks>
/// <example>
/// <code>
/// var car = await client.GetCarAsync(vin);
/// await car.UpdateRealtimeAsync();
/// await car.Lock.LockAsync();
/// await car.Hvac.StartAsync(temperature: 21, duration: 20);
/// var lockState = car.State.Realtime?.LeftFrontDoorLock; // LockState.Locked, projected
/// </code>
/// </example>
public sealed partial class Car
{
    /// <summary>The vehicle identification number.</summary>
    public string Vin { get; }
}
