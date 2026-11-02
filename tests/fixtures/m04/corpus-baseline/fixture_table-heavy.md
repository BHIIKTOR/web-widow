The table lists measured values from the 2026 bench run. All sensors were powered from a linear regulator and sampled at 1 Hz.

| Sensor  | Range        | Accuracy | Interface |
| ------- | ------------ | -------- | --------- |
| BME280  | 300–1100 hPa | ±1 hPa   | I²C       |
| SCD41   | 400–5000 ppm | ±50 ppm  | I²C       |
| DS18B20 | −55–125 °C   | ±0.5 °C  | 1-Wire    |
| SHT45   | 0–100 %RH    | ±1.8 %RH | I²C       |

The SCD41 needs a background calibration period of at least seven days before its readings are trustworthy.