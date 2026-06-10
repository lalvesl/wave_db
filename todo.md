# TO DO

- Falta de clareza sobre aquizição de dados por clients;

* Remove serde,postcard crates from this repository, create own implementation of serde

* In query there's an implementation of enum @crates/wavedb/src/query.rs#L39-53 to describe data to quering, add all types of number f|u|i/8|16|32|64|128;

# DOING

# DONE

- read the @readme.md and undestand this project. There is a problem with expressions, i need to write the name of column in str, i want to replace this with enum os each column. Take as much time as you need!
- The description on @readme.md#L17-28 is not describe correcly this project, read again the @readme.md and describe the problems of common sql, mixing data of all users, and mixing data of elements not reletead (the NestedNonUnique) when data are storege and searcheable only with interested data, reducing cache and diskIOps;
