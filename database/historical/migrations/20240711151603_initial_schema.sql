-- Add migration script here
CREATE TABLE IF NOT EXISTS instrument (
  id SERIAL PRIMARY KEY,
  ticker VARCHAR(10) NOT NULL UNIQUE,
  name VARCHAR(25) NOT NULL,
  vendor VARCHAR(50) NOT NULL DEFAULT 'unknown',
  stype VARCHAR(50),
  dataset VARCHAR(50),
  last_available BIGINT NOT NULL DEFAULT 0,
  first_available BIGINT NOT NULL DEFAULT 0,
  active BOOL NOT NULL DEFAULT TRUE
);

CREATE TABLE IF NOT EXISTS mbp (
  id SERIAL PRIMARY KEY,
  instrument_id INTEGER NOT NULL, -- corresponds to uint32_t
  ts_event BIGINT NOT NULL, -- corresponds to uint64_t, stored as TIMESTAMP in QuestDB
  price BIGINT NOT NULL, -- corresponds to float64
  size INTEGER NOT NULL, -- corresponds to uint32_t
  action INTEGER NOT NULL, -- corresponds to char
  side INTEGER NOT NULL, -- corresponds to char
  flags INTEGER NOT NULL,
  ts_recv BIGINT NOT NULL, -- corresponds to uint64_t, stored as TIMESTAMP in QuestDB
  ts_in_delta INTEGER NOT NULL, -- corresponds to int32_t
  sequence INTEGER NOT NULL, -- corresponds to uint32_t
  discriminator INTEGER NOT NULL,
  order_book_hash VARCHAR NOT NULL,
  CONSTRAINT fk_instrument_mbp
    FOREIGN KEY(instrument_id) 
      REFERENCES instrument(id)
      ON DELETE CASCADE,
  CONSTRAINT unique_instrument_ts_sequence_event UNIQUE (instrument_id, ts_event, price, size, flags, sequence, order_book_hash, ts_recv, action, side, discriminator)
);

CREATE TABLE IF NOT EXISTS bid_ask (
  id SERIAL PRIMARY KEY,
  mbp_id INTEGER NOT NULL, -- Foreign key to mbp_1
  depth INTEGER NOT NULL, -- Depth level in the order book
  bid_px BIGINT NOT NULL, -- Bid price
  bid_sz INTEGER NOT NULL, -- Bid size
  bid_ct INTEGER NOT NULL, -- Bid order count
  ask_px BIGINT NOT NULL, -- Ask price
  ask_sz INTEGER NOT NULL, -- Ask size
  ask_ct INTEGER NOT NULL, -- Ask order count
  CONSTRAINT fk_mbp_bid_ask
    FOREIGN KEY(mbp_id) 
      REFERENCES mbp(id)
      ON DELETE CASCADE
);

-- Create the indexes
CREATE INDEX idx_mbp_instrument_ts_event ON mbp (instrument_id, ts_event);
CREATE INDEX idx_bid_ask_mbp_id_depth ON bid_ask (mbp_id, depth);

-- Function for Bbo queries 
CREATE FUNCTION coalesce_r_sfunc(state anyelement, value anyelement)
RETURNS anyelement
IMMUTABLE PARALLEL SAFE
AS $$
    SELECT COALESCE(value, state);
$$ LANGUAGE sql;

CREATE AGGREGATE find_last_ignore_nulls(anyelement) (
    SFUNC = coalesce_r_sfunc,
    STYPE = anyelement
);
