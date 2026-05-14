create table if not exists ValidPaths (
    id               integer primary key autoincrement not null,
    path             text unique not null,
    hash             text not null,
    registrationTime integer not null,
    deriver          text,
    narSize          integer,
    ultimate         integer,
    sigs             text,
    ca               text
);

create table if not exists Refs (
    referrer  integer not null,
    reference integer not null,
    primary key (referrer, reference),
    foreign key (referrer) references ValidPaths(id) on delete cascade,
    foreign key (reference) references ValidPaths(id) on delete restrict
);

create index if not exists IndexReference on Refs(reference);

create trigger if not exists DeleteSelfRefs before delete on ValidPaths
  begin
    delete from Refs where referrer = old.id and reference = old.id;
  end;

create table if not exists DerivationOutputs (
    drv  integer not null,
    id   text not null,
    path text not null,
    primary key (drv, id),
    foreign key (drv) references ValidPaths(id) on delete cascade
);

create index if not exists IndexDerivationOutputs on DerivationOutputs(path);
